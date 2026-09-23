"""设备档位：显存空出来就静默升舱，显卡崩了就静默退回内存。python test_device.py"""
import json
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import engine
from engine import DeviceState, device_tick, is_gpu_failure, resident_need_mb


def settings(**kw):
    """把 load_settings 换成固定的一份。默认：开自动切换、起步在显卡上。

    DeviceState 每次判断都重读设置，所以测试必须控制住这个输入，
    否则结果会跟着用户当前的真实开关状态飘。
    """
    base = {"autoDeviceEnabled": True, "cpuOnlyEnabled": False}
    base.update(kw)
    return patch("engine.load_settings", return_value=base)


class DeviceStateTests(unittest.TestCase):
    def test_follows_the_setting_when_auto_is_off(self):
        d = DeviceState()
        with settings(autoDeviceEnabled=False, cpuOnlyEnabled=True):
            self.assertTrue(d.cpu_only())
        with settings(autoDeviceEnabled=False, cpuOnlyEnabled=False):
            self.assertFalse(d.cpu_only())

    def test_engine_may_not_override_when_auto_is_off(self):
        """关掉自动切换是用户的明确表态，引擎不许自作主张。"""
        d = DeviceState()
        with settings(autoDeviceEnabled=False, cpuOnlyEnabled=False):
            self.assertFalse(d.demote("显存炸了"))
            self.assertFalse(d.cpu_only(), "关掉自动切换后引擎照样改了档位")
            self.assertFalse(d.promote("显存空了"))

    def test_demote_then_promote(self):
        d = DeviceState()
        with settings(cpuOnlyEnabled=False):
            self.assertTrue(d.demote("炸了"))
            self.assertTrue(d.cpu_only())
            self.assertTrue(d.promote("空了"))
            self.assertFalse(d.cpu_only())

    def test_user_flipping_the_switch_clears_the_engine_override(self):
        """用户在设置页动开关是明确指令，引擎自己的判断必须让位。
        否则用户点了「用内存跑模型」发现没反应——那比不做这功能更糟。"""
        d = DeviceState()
        with settings(cpuOnlyEnabled=False):
            d.cpu_only()                     # 先记下用户此刻选的是显卡
            d.demote("炸了")
            self.assertTrue(d.cpu_only())
        with settings(cpuOnlyEnabled=True):
            self.assertTrue(d.cpu_only())    # 用户自己也选了内存
        with settings(cpuOnlyEnabled=False):
            self.assertFalse(d.cpu_only(), "用户把开关扳回显卡，引擎的降级还压在上面")

    def test_demote_is_idempotent(self):
        """连续降级不该刷屏，也不该把状态搞乱。"""
        d = DeviceState()
        with settings():
            self.assertTrue(d.demote("第一次"))
            self.assertTrue(d.demote("第二次"))
            self.assertTrue(d.cpu_only())


class PromoteVoteTests(unittest.TestCase):
    def test_needs_consecutive_samples(self):
        """显存是别的程序在占，它出完一张图的间隙会短暂空出一大块。
        撞上那个瞬间就搬家，下一秒又被抢回去，于是来回搬。"""
        d = DeviceState()
        with settings():
            for _ in range(engine.GPU_PROMOTE_SAMPLES - 1):
                self.assertFalse(d.vote(True))
            self.assertTrue(d.vote(True))

    def test_a_single_miss_resets_the_streak(self):
        d = DeviceState()
        with settings():
            for _ in range(engine.GPU_PROMOTE_SAMPLES - 1):
                d.vote(True)
            d.vote(False)
            for _ in range(engine.GPU_PROMOTE_SAMPLES - 1):
                self.assertFalse(d.vote(True), "中间断了一次，连击却没清零")
            self.assertTrue(d.vote(True))

    def test_a_demote_does_not_park_the_engine_in_ram(self):
        """降舱之后不该再额外压一段等待——投够票就得放行。

        这里曾经有个 600 秒的冷静期。真机日志上它的样子是：每一次升舱都精确落在
        降舱后 10.5 分钟，而那一刻空闲显存有 13.8~14.8 GB，早就够了。ComfyUI 出完
        图两分钟就把显存吐了出来，我们却还在内存里把剩下八分钟慢腾腾耗完。
        防抖不靠等待，靠的是 need 里已经算进了我们自己那份权重，
        见 test_a_demote_cannot_immediately_bounce_back。
        """
        d = DeviceState()
        with settings():
            d.demote("炸了")
            for _ in range(engine.GPU_PROMOTE_SAMPLES - 1):
                self.assertFalse(d.vote(True), "还没投够票就放行了")
            self.assertTrue(d.vote(True), "连续投够票还不放行——冷静期又回来了")


class MarkerTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = Path(tmp.name) / "gpu-demoted"
        p = patch("engine.GPU_DEMOTE_MARKER", self.path)
        p.start()
        self.addCleanup(p.stop)

    def test_fresh_marker_starts_on_cpu(self):
        """看门狗杀进程前留下的标记，让重启后的新进程先用内存跑，
        而不是一头撞回同一堵墙、反复被杀。"""
        with settings():
            DeviceState().note_gpu_death("ASR 推理卡死")
            fresh = DeviceState()
            fresh.adopt_marker()
            self.assertTrue(fresh.cpu_only())

    def test_stale_marker_is_ignored_and_removed(self):
        self.path.write_text(json.dumps({"at": time.time() - engine.GPU_DEMOTE_TTL_S - 60,
                                         "why": "很久以前"}), encoding="utf-8")
        with settings():
            d = DeviceState()
            d.adopt_marker()
            self.assertFalse(d.cpu_only(), "过期标记还把引擎摁在内存里")
        self.assertFalse(self.path.exists(), "过期标记没删掉，会被一读再读")

    def test_marker_from_the_future_is_treated_as_stale(self):
        """用户改过系统时间。宁可多试一次显卡，也别永远锁在内存里。"""
        self.path.write_text(json.dumps({"at": time.time() + 99999, "why": "未来"}),
                             encoding="utf-8")
        with settings():
            d = DeviceState()
            d.adopt_marker()
            self.assertFalse(d.cpu_only())

    def test_promote_clears_the_marker(self):
        with settings():
            d = DeviceState()
            d.note_gpu_death("炸了")
            d.promote("空了")
        self.assertFalse(self.path.exists(), "搬回显卡了，崩溃标记却还留着")

    def test_garbage_marker_does_not_crash_startup(self):
        self.path.write_text("这不是 JSON", encoding="utf-8")
        with settings():
            d = DeviceState()
            d.adopt_marker()
            self.assertFalse(d.cpu_only())

    def test_no_marker_at_all_is_fine(self):
        with settings():
            d = DeviceState()
            d.adopt_marker()
            self.assertFalse(d.cpu_only())


class GpuFailureTests(unittest.TestCase):
    def test_recognises_what_each_backend_actually_throws(self):
        """三个后端报显存耗尽的长相完全不同，认的是字符串不是异常类。"""
        for exc in (RuntimeError("CUDA out of memory. Tried to allocate 2.00 GiB"),
                    RuntimeError("ggml_backend_cuda_buffer_type_alloc_buffer: failed to allocate"),
                    RuntimeError("CUDA error: an illegal memory access was encountered"),
                    RuntimeError("cuBLAS API failed with status 15"),
                    RuntimeError("no kernel image is available for execution")):
            self.assertTrue(is_gpu_failure(exc), f"没认出来: {exc}")

    def test_class_name_alone_is_enough(self):
        """torch 抛的是 OutOfMemoryError，消息里未必带 'out of memory'。"""
        class OutOfMemoryError(RuntimeError):
            pass
        self.assertTrue(is_gpu_failure(OutOfMemoryError("显存没了")))

    def test_does_not_swallow_unrelated_errors(self):
        """认漏了只是慢一点；认多了会把真 bug 当显存问题，降级重跑一遍还是错，
        而且从此赖在内存里跑，用户只觉得"忽然变慢了"却查不出原因。"""
        for exc in (ValueError("音频为空"), KeyError("refineModel"),
                    FileNotFoundError("模型文件不在"), TimeoutError("等太久")):
            self.assertFalse(is_gpu_failure(exc), f"误判成显存问题: {exc}")


class NeedTests(unittest.TestCase):
    def test_counts_the_asr_alone_plus_headroom(self):
        """常驻的只剩 ASR 一个模型。学词那个 4B 是用户点一下才跑的一次性活，
        跑完就卸；把它也算进升舱门槛，这台机器就永远升不上去。"""
        seen = []

        class FakeAsr:
            path = Path("asr")

        def spy(stage, path=None):
            seen.append(stage)
            return 1000

        with patch("engine.stage_estimate_mb", spy):
            self.assertEqual(resident_need_mb({}, FakeAsr()),
                             1000 + engine.GPU_PROMOTE_HEADROOM_MB)
        self.assertEqual(seen, ["asr"], "门槛里混进了不会常驻的模型")

    def test_headroom_must_clear_the_demote_floor(self):
        """升舱余量必须明显高过降舱地板线。

        搬进显存之后剩下的空闲量大约就是这个余量本身；要是它不比
        ASR_FREE_FLOOR_MB 高，引擎会刚搬完就立刻触发降舱，权重在内存和显存之间
        反复横跳，比一直待在内存里还慢得多。"""
        self.assertGreater(engine.GPU_PROMOTE_HEADROOM_MB, engine.ASR_FREE_FLOOR_MB,
                           "升舱余量不高于降舱地板线，升上去马上就会掉下来")
        self.assertGreaterEqual(
            engine.GPU_PROMOTE_HEADROOM_MB - engine.ASR_FREE_FLOOR_MB, 500,
            "余量和地板线贴得太近，别的程序抢回一点显存就来回搬")


class SnapshotTests(unittest.TestCase):
    """托盘那行「现在跑在哪」的数据源。"""

    def test_reports_the_actual_tier_not_the_setting(self):
        """这是整个功能的意义所在：引擎自动降舱之后，用户选的还是「显存」，
        但模型实际在内存里。托盘要显示的是后者——不然用户对着一个写着「显存」
        的托盘纳闷为什么这么慢，而真相就在他看不到的地方。"""
        d = DeviceState()
        with settings(cpuOnlyEnabled=False):
            self.assertEqual(d.snapshot()["at"], "gpu")
            d.demote("显存被 ComfyUI 占满了")
            s = d.snapshot()
            self.assertEqual(s["at"], "cpu", "引擎降到内存了，快照还报显存")
            self.assertEqual(s["manual"], "gpu", "用户选的那档被覆盖掉了")

    def test_explains_itself_only_when_the_engine_actually_stepped_in(self):
        d = DeviceState()
        with settings(cpuOnlyEnabled=False):
            s = d.snapshot()
            self.assertFalse(s["forced"], "引擎没插手也报成引擎改的")
            self.assertEqual(s["why"], "", "没插手却带了原因，托盘会多显示一句噪音")
            d.demote("显存被占满了")
            s = d.snapshot()
            self.assertTrue(s["forced"])
            self.assertIn("显存被占满了", s["why"], "改了档却不给原因，用户只会以为开关坏了")

    def test_auto_off_means_the_engine_never_speaks(self):
        """关掉自动切换后 cpu_only() 压根不看 override。这时候还报「引擎改的」，
        托盘就会对着一个没在生效的判断解释半天，而实际档位明明是用户自己选的。"""
        d = DeviceState()
        with settings(cpuOnlyEnabled=False):
            d.demote("炸了")
        with settings(autoDeviceEnabled=False, cpuOnlyEnabled=False):
            s = d.snapshot()
            self.assertEqual(s["at"], "gpu", "关了自动切换，引擎的降级还压在上面")
            self.assertFalse(s["forced"], "override 没在生效却报成引擎改的")
            self.assertEqual(s["why"], "")
            self.assertFalse(s["auto"])

    def test_manual_tracks_the_setting_both_ways(self):
        d = DeviceState()
        with settings(cpuOnlyEnabled=True):
            self.assertEqual(d.snapshot()["manual"], "cpu")
        with settings(cpuOnlyEnabled=False):
            self.assertEqual(d.snapshot()["manual"], "gpu")

    def test_is_json_safe(self):
        """要原样走 websocket 送到渲染进程、再转给主进程。塞进去不可序列化的东西
        会让整条 pong 静默失败——托盘从此停在「等待引擎」，而日志里什么都没有。"""
        d = DeviceState()
        with settings():
            d.demote("炸了")
            s = d.snapshot()
            self.assertEqual(json.loads(json.dumps(s)), s)
            self.assertEqual(set(s), {"at", "manual", "auto", "forced", "why"})


class FakeAsr:
    def __init__(self):
        self.path = Path("asr")
        self.unloaded = 0

    async def unload(self):
        self.unloaded += 1


class FakeLlm:
    def __init__(self):
        self.unloaded = 0

    async def unload(self):
        self.unloaded += 1


class FakeScheduler:
    def __init__(self, active=None, waiters=None):
        self.active = active or {}
        self.waiters = waiters or []


class FakeProvider:
    def __init__(self, free_mb, total_mb=16376):
        self.free_mb, self.total_mb = free_mb, total_mb

    async def sample(self):
        return self.total_mb * 1024 * 1024, self.free_mb * 1024 * 1024


class TickTests(unittest.IsolatedAsyncioTestCase):
    NEED = 6000

    def setUp(self):
        self.asr, self.llm, self.device = FakeAsr(), FakeLlm(), DeviceState()
        self.warmed = 0

        async def fake_warmup(asr, llm):
            self.warmed += 1

        for p in (patch("engine.DEVICE", self.device),
                  patch("engine.resident_need_mb", return_value=self.NEED),
                  patch("engine.warmup", fake_warmup)):
            p.start()
            self.addCleanup(p.stop)

    async def _tick(self, free_mb, scheduler=None):
        with patch("engine.GPU_PROVIDER", FakeProvider(free_mb)):
            return await device_tick(self.asr, self.llm, scheduler or FakeScheduler())

    async def test_promotes_after_a_sustained_run_then_reseats_and_warms(self):
        with settings(cpuOnlyEnabled=True):
            for _ in range(engine.GPU_PROMOTE_SAMPLES - 1):
                self.assertIsNone(await self._tick(self.NEED + 1000))
                self.assertTrue(self.device.cpu_only(), "还没连够次数就搬家了")
            self.assertEqual(await self._tick(self.NEED + 1000), "promote")
            self.assertFalse(self.device.cpu_only())
        # 换档之后两边权重都得丢掉，否则档位改了、模型还在原来的设备上跑。
        self.assertEqual(self.llm.unloaded, 1, "LLM 没卸载，档位改了也白改")
        self.assertEqual(self.asr.unloaded, 1,
                         "ASR 没卸载，档位改了权重还留在原设备上")
        self.assertEqual(self.warmed, 1,
                         "升舱后没预热：搬回显卡要拖到用户下次开口才真的发生")

    async def test_a_demote_cannot_immediately_bounce_back(self):
        """降舱之后不该立刻又够格升舱。这是去掉冷静期之后唯一的防抖依据。

        降舱要求 free < ASR_FREE_FLOOR_MB；卸掉权重 W 后空闲变成 free+W，而升舱
        要求 free+W >= need = W + GPU_PROMOTE_HEADROOM_MB，也就是 free >= 2200。
        两者不可能同时成立——**前提是余量一直比地板线大**。把 HEADROOM 调到比
        FLOOR 还低，降完立刻就够升，模型会在内存和显存之间反复横跳。
        """
        self.assertGreater(engine.GPU_PROMOTE_HEADROOM_MB, engine.ASR_FREE_FLOOR_MB,
                           "升舱余量比降舱地板线还低，降完立刻就够升，会反复横跳")
        weights = self.NEED - engine.GPU_PROMOTE_HEADROOM_MB
        low = engine.ASR_FREE_FLOOR_MB - 1
        with settings(cpuOnlyEnabled=False):
            self.assertEqual(await self._tick(low), "demote")
            # 权重回了内存，卡上多出 weights 那么多空闲。多投几轮票也不该动。
            for _ in range(engine.GPU_PROMOTE_SAMPLES + 1):
                self.assertIsNone(await self._tick(low + weights),
                                  "刚降完就又搬回显卡了——防抖没了")

    async def test_promoting_at_the_exact_threshold_does_not_bounce_back(self):
        """防振荡：卡着门槛线搬进显存，下一轮不能立刻又被赶回内存。

        搬完之后剩下的空闲量就是 GPU_PROMOTE_HEADROOM_MB；它要是不比
        ASR_FREE_FLOOR_MB 高，这里就会看到 promote 紧跟一个 demote，
        真机上表现为权重在内存和显存之间反复横跳。
        """
        weights = self.NEED - engine.GPU_PROMOTE_HEADROOM_MB
        with settings(cpuOnlyEnabled=True):
            got = None
            for _ in range(engine.GPU_PROMOTE_SAMPLES):
                got = await self._tick(self.NEED)      # 空闲量正好卡在门槛上
            self.assertEqual(got, "promote")
            # 权重进了显卡，空闲量掉到只剩当初留的那段余量。
            after = self.NEED - weights
            self.assertIsNone(await self._tick(after),
                              f"刚升舱、空闲还剩 {after} MB 就被赶回内存了")
            self.assertFalse(self.device.cpu_only())

    async def test_promotion_works_while_running_on_cpu(self):
        """free_vram_mb() 在 CPU 模式下返回 None。监测要是图省事用了它，
        就永远看不见显存空出来，"自动搬回显卡"整个功能等于不存在。"""
        with settings(cpuOnlyEnabled=True):
            self.assertTrue(self.device.cpu_only())
            got = None
            for _ in range(engine.GPU_PROMOTE_SAMPLES):
                got = await self._tick(self.NEED + 1000)
            self.assertEqual(got, "promote")

    async def test_never_promotes_when_vram_is_short(self):
        with settings(cpuOnlyEnabled=True):
            for _ in range(engine.GPU_PROMOTE_SAMPLES * 2):
                self.assertIsNone(await self._tick(self.NEED - 1))
            self.assertTrue(self.device.cpu_only())
            self.assertEqual(self.warmed, 0)

    async def test_demotes_when_free_vram_falls_through_the_floor(self):
        with settings(cpuOnlyEnabled=False):
            self.assertEqual(await self._tick(engine.ASR_FREE_FLOOR_MB - 1), "demote")
            self.assertTrue(self.device.cpu_only())
        self.assertEqual(self.asr.unloaded, 1)
        self.assertEqual(self.llm.unloaded, 1)

    async def test_stays_on_gpu_while_there_is_room(self):
        with settings(cpuOnlyEnabled=False):
            self.assertIsNone(await self._tick(engine.ASR_FREE_FLOOR_MB + 1))
            self.assertFalse(self.device.cpu_only())

    async def test_does_nothing_while_a_dictation_is_in_flight(self):
        """换档要卸载再重载权重，正在跑的那一轮会当场崩掉。"""
        busy = FakeScheduler(active={("job", "asr"): (0, "shared", 0.0)})
        with settings(cpuOnlyEnabled=True):
            for _ in range(engine.GPU_PROMOTE_SAMPLES * 2):
                self.assertIsNone(await self._tick(self.NEED + 1000, busy))
            self.assertTrue(self.device.cpu_only())
            self.assertEqual(self.asr.unloaded, 0, "有活在跑还是把权重卸了")

    async def test_does_nothing_while_someone_is_queued(self):
        busy = FakeScheduler(waiters=[object()])
        with settings(cpuOnlyEnabled=True):
            for _ in range(engine.GPU_PROMOTE_SAMPLES * 2):
                self.assertIsNone(await self._tick(self.NEED + 1000, busy))

    async def test_respects_the_user_turning_auto_off(self):
        with settings(autoDeviceEnabled=False, cpuOnlyEnabled=True):
            for _ in range(engine.GPU_PROMOTE_SAMPLES * 2):
                self.assertIsNone(await self._tick(self.NEED + 1000))
            self.assertTrue(self.device.cpu_only())

    async def test_no_nvidia_card_means_stay_put(self):
        class Blind:
            async def sample(self):
                return None

        with settings(cpuOnlyEnabled=True), patch("engine.GPU_PROVIDER", Blind()):
            self.assertIsNone(await device_tick(self.asr, self.llm, FakeScheduler()))
            self.assertTrue(self.device.cpu_only())


class ResidentNeedTests(unittest.TestCase):
    """升舱门槛只能算真会进显存的东西。

    多算一个不会加载的模型，后果不是"保守一点"而是"这台机器永远升不了舱"：
    降舱只要空闲跌破 1400 MB，升舱却要 asr+llm+2200。多出来的那一项在 16 GB 卡上
    能把门槛顶到 14 GB，两道坎之间隔着 10 倍，中间跨不过去。
    """

    class FakeAsr:
        path = None      # _model_mb 读不到就走 900 的兜底值，正好不依赖真权重

    def test_threshold_is_the_asr_and_nothing_else(self):
        asr = self.FakeAsr()
        self.assertEqual(resident_need_mb({}, asr),
                         engine.stage_estimate_mb("asr", asr.path)
                         + engine.GPU_PROMOTE_HEADROOM_MB)

    def test_no_llm_setting_can_move_the_threshold(self):
        """设置里还剩一个文本模型，但它只给学词用，一次性加载完就卸。
        任何和它有关的开关都不该再挪动这条线。"""
        asr = self.FakeAsr()
        base = resident_need_mb({}, asr)
        for st in ({"learnModel": "qwen3-4b"}, {"learnModel": "qwen3-8b"},
                   {"learnNamesFromEdits": False}, {"learnNamesFromEdits": True}):
            self.assertEqual(resident_need_mb(st, asr), base, st)

class WarmupTests(unittest.IsolatedAsyncioTestCase):
    """预热只能把真会被用到的模型塞进显存。

    llama.cpp 预热完不会卸，权重就一直占着好几个 G，挤掉的正好是 ASR 的常驻
    名额——线上日志里那句"显存只剩 5976 MB（常驻需要 6382 MB），跳过 ASR 预热"
    就是这么来的。
    """

    class FakeLlm:
        def __init__(self):
            self.chats = 0

        async def chat(self, *a, **kw):
            self.chats += 1
            return "ok"

    class FakeAsr:
        model_id = "fake-asr"
        path = None

        def __init__(self):
            self.preloads = 0

        async def preload(self):
            self.preloads += 1

    async def _warmup(self, **st):
        llm, asr = self.FakeLlm(), self.FakeAsr()
        # asrKeepLoaded 关掉：这几条只盯 LLM 那一侧，别让 ASR 预热的显存判断掺进来。
        base = {"asrKeepLoaded": False}
        base.update(st)
        with patch("engine.load_settings", return_value=base):
            await engine.warmup(asr, llm)
        return llm, asr

    async def test_never_warms_an_llm(self):
        """预热完 llama.cpp 不会卸，权重就一直占着。听写链路已经不碰 LLM 了，
        预热还去装一个，白占的那几个 G 挤掉的正好是 ASR 的常驻名额。"""
        for st in ({}, {"learnNamesFromEdits": True}, {"learnModel": "qwen3-4b"}):
            llm, _ = await self._warmup(**st)
            self.assertEqual(llm.chats, 0, st)

class DeviceReportTests(unittest.IsolatedAsyncioTestCase):
    """托盘那一行的数据源。"""

    NEED = 7182

    def setUp(self):
        self.device = DeviceState()
        for p in (patch("engine.DEVICE", self.device),
                  patch("engine.resident_need_mb", return_value=self.NEED),
                  patch.dict(engine._VRAM_VIEW, {"at": 0.0, "data": {}})):
            p.start()
            self.addCleanup(p.stop)

    async def test_reports_live_free_vram_not_the_frozen_demote_reason(self):
        """余量必须是现读的，不能是降舱那一刻写进 why 的那个。

        真机上出过的样子：托盘挂着"空闲显存只剩 895 MB"，任务管理器同一刻显示
        2.6/16.0 GB。数字本身没错，是三小时前的——而它用现在时的口气一直挂着。
        """
        with settings(cpuOnlyEnabled=False):
            self.device.demote("空闲显存只剩 895 MB")
            with patch("engine.GPU_PROVIDER", FakeProvider(13400)):
                r = await engine.device_report(FakeAsr())
        self.assertEqual(r["at"], "cpu")
        self.assertEqual(r["freeMb"], 13400, "报的还是降舱那一刻冻结下来的余量")

    async def test_carries_the_same_threshold_the_engine_promotes_on(self):
        """托盘说"够了"而引擎不动，比干脆不显示更糟。"""
        with settings(cpuOnlyEnabled=True):
            with patch("engine.GPU_PROVIDER", FakeProvider(5000)):
                r = await engine.device_report(FakeAsr())
        self.assertEqual(r["needMb"], self.NEED, "门槛不是 resident_need_mb 算的那个")

    async def test_omits_the_number_when_there_is_no_nvidia_card(self):
        """探不到就一个字都别写。补个 0 会让用户以为显存被占满了。"""
        class Dead:
            async def sample(self):
                return None

        with settings():
            with patch("engine.GPU_PROVIDER", Dead()):
                r = await engine.device_report(FakeAsr())
        self.assertNotIn("freeMb", r)
        self.assertNotIn("needMb", r)

    async def test_is_json_safe(self):
        """整份要原样走 websocket 再转给主进程；塞进去不可序列化的东西，
        整条 pong 会静默失败，托盘从此停在「等待引擎」而日志里什么都没有。"""
        with settings():
            with patch("engine.GPU_PROVIDER", FakeProvider(9000)):
                r = await engine.device_report(FakeAsr())
        self.assertEqual(json.loads(json.dumps(r)), r)


class FakeNvml:
    """够 encoder_pids_sync 用的假 NVML：只实现它真正会调的那一个函数。

    ctypes.byref(x)._obj is x，所以假件能直接往调用方那个 c_uint 里写个数，
    不必为了可测而去改生产代码的签名。
    """

    def __init__(self, pids, count_rc=0, fetch_rc=0):
        self.pids, self.count_rc, self.fetch_rc = pids, count_rc, fetch_rc
        self.calls = 0

    def nvmlDeviceGetEncoderSessions(self, dev, count_ref, buf):
        self.calls += 1
        if buf is None:
            count_ref._obj.value = len(self.pids)
            return self.count_rc
        for i, pid in enumerate(self.pids):
            buf[i].pid = pid
        return self.fetch_rc


class EncoderSessionTests(unittest.TestCase):
    """谁在用显卡编码视频——悬浮麦克风靠它判断「是不是正在被远程串流」。

    这块唯一真正危险的地方是 None 和 [] 的区别，见下面第二个用例。
    """

    def provider(self, nvml):
        # 绕开 __init__：它会去真的 WinDLL 一个 nvml.dll，跑 CI 或者换台机器就崩。
        p = engine.GpuMemoryProvider.__new__(engine.GpuMemoryProvider)
        p._nvml, p._device = nvml, object()
        return p

    def test_reports_the_pids_that_are_encoding(self):
        self.assertEqual(self.provider(FakeNvml([6056, 1234])).encoder_pids_sync(), [6056, 1234])

    def test_no_session_is_an_empty_list_not_none(self):
        """「确实没人在编码」和「问不出来」必须是两个值。

        混成一个的后果是把 fail-open 拆了：main.js 拿 None 当「没在推流」，
        于是没有 N 卡的机器上、引擎刚起来还没读到的那几秒里，悬浮麦克风会被
        判成「人坐在主机前」而藏起来——而那正是用户没有键盘可以补救的时候。
        """
        nvml = FakeNvml([])
        got = self.provider(nvml).encoder_pids_sync()
        self.assertEqual(got, [])
        self.assertIsNotNone(got)
        self.assertEqual(nvml.calls, 1, "数出 0 个会话之后不该再去取一次数组")

    def test_insufficient_size_on_the_count_probe_still_counts(self):
        """buf 传 None 只问个数时，驱动可以合法地回 INSUFFICIENT_SIZE(7)。

        把它当失败，整条判据在那种驱动上就永远是「问不出来」。
        """
        self.assertEqual(self.provider(FakeNvml([6056], count_rc=7)).encoder_pids_sync(), [6056])

    def test_unexpected_rc_is_unknown(self):
        self.assertIsNone(self.provider(FakeNvml([6056], count_rc=999)).encoder_pids_sync())

    def test_fetch_failure_is_unknown(self):
        self.assertIsNone(self.provider(FakeNvml([6056], fetch_rc=999)).encoder_pids_sync())

    def test_no_nvml_is_unknown(self):
        """没有 N 卡的机器：NVML 根本没加载，这时候只能说「不知道」。"""
        self.assertIsNone(self.provider(None).encoder_pids_sync())

    def test_a_throwing_driver_is_unknown(self):
        """驱动抛异常也只是「问不出来」，不能把这条 pong 整个带崩——
        它同一条消息里还驮着托盘要的档位。"""

        class Boom:
            def nvmlDeviceGetEncoderSessions(self, *a):
                raise OSError("driver went away")

        self.assertIsNone(self.provider(Boom()).encoder_pids_sync())

if __name__ == "__main__":
    unittest.main(verbosity=1)
