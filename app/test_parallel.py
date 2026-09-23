"""Concurrency foundations: tagged protocol and balanced FIFO GPU reservations."""
import asyncio
import json
import unittest
from pathlib import Path
from unittest.mock import patch

from engine import GpuScheduler, decode_audio_frame, encode_audio_frame, handle


# cpu_only() 每次都重读用户真实的设置文件。用户把"用内存跑模型"打开后，
# 调度器会跳过显存准入，四个排队测试就会莫名其妙地挂——测试不该依赖用户当前
# 的开关状态。默认按显存模式跑，需要 CPU 语义的用例自己 patch。
_cpu_patch = None


def setUpModule():
    global _cpu_patch
    _cpu_patch = patch("engine.cpu_only", return_value=False)
    _cpu_patch.start()


def tearDownModule():
    if _cpu_patch:
        _cpu_patch.stop()


class FakeProvider:
    def __init__(self, total_mb=1000, free_mb=1000):
        self.total = total_mb * 1024 * 1024
        self.free = free_mb * 1024 * 1024
        self.samples = 0

    async def sample(self):
        self.samples += 1
        return self.total, self.free


class ProtocolTests(unittest.TestCase):
    def test_tagged_frame_round_trip(self):
        frame = encode_audio_frame("job-a", b"RIFFaudio")
        self.assertEqual(decode_audio_frame(frame), ("job-a", b"RIFFaudio"))

    def test_legacy_frame_is_explicitly_untagged(self):
        self.assertEqual(decode_audio_frame(b"RIFFaudio"), (None, b"RIFFaudio"))

    def test_truncated_tagged_frame_is_rejected(self):
        with self.assertRaises(ValueError):
            decode_audio_frame(b"LLAF\x01\x08short")


class SchedulerTests(unittest.IsolatedAsyncioTestCase):
    async def test_release_wakes_fifo_waiter_and_subtracts_reservation(self):
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        first = await scheduler.acquire("a", "asr", 500)
        second_task = asyncio.create_task(scheduler.acquire("b", "asr", 500))
        await asyncio.sleep(0.02)
        self.assertEqual(scheduler.status()["queued"], 1)
        self.assertEqual(scheduler.status()["reserved_mb"], 500)
        await first.release()
        second = await asyncio.wait_for(second_task, 1)
        self.assertEqual(scheduler.status()["reserved_mb"], 500)
        await second.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)
        self.assertGreaterEqual(scheduler.provider.samples, 4)

    async def test_cancelled_waiter_does_not_leak(self):
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        first = await scheduler.acquire("a", "asr", 800)
        blocked = asyncio.create_task(scheduler.acquire("b", "refine", 200))
        await asyncio.sleep(0.02)
        blocked.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await blocked
        self.assertEqual(scheduler.status()["queued"], 0)
        await first.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)

    async def test_queued_notification_failure_does_not_block_admission(self):
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        first = await scheduler.acquire("a", "asr", 800)

        async def broken_notification(_status, _estimate):
            raise RuntimeError("socket closed")

        blocked = asyncio.create_task(
            scheduler.acquire(
                "b",
                "refine",
                200,
                on_queued=broken_notification,
            )
        )
        await asyncio.sleep(0.02)
        self.assertEqual(scheduler.status()["queued"], 1)
        await first.release()
        second = await asyncio.wait_for(blocked, 1)
        await second.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)

    async def test_release_is_idempotent(self):
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        ticket = await scheduler.acquire("a", "asr", 100)
        await ticket.release()
        await ticket.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)

    async def test_sole_stage_is_admitted_when_free_vram_is_below_estimate(self):
        # 常驻模型吃掉显存后 free 长期低于估算；没有活动票就没人 release，
        # 死等 = 药丸永远停在最终输出。唯一任务必须放行。
        scheduler = GpuScheduler(FakeProvider(total_mb=16376, free_mb=2677))
        scheduler.reserve_percent = lambda: 15
        ticket = await asyncio.wait_for(scheduler.acquire("a", "refine", 2500), 1)
        await ticket.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)

    async def test_queued_stage_recovers_when_third_party_frees_vram(self):
        # 显存被别的进程占着时没有 release 来唤醒队列。等待必须自己超时重采样，
        # 否则 thinking 永久挂起。
        provider = FakeProvider(total_mb=16376, free_mb=1000)
        scheduler = GpuScheduler(provider)
        scheduler.reserve_percent = lambda: 15
        held = await scheduler.acquire("a", "asr", 500)

        blocked = asyncio.create_task(scheduler.acquire("b", "refine", 3000))
        await asyncio.sleep(0.05)
        self.assertEqual(scheduler.status()["queued"], 1)

        # 第三方释放显存，但 scheduler 收不到任何 release 通知。
        provider.free = 12000 * 1024 * 1024
        ticket = await asyncio.wait_for(blocked, 15)
        await ticket.release()
        await held.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)


    async def test_double_cancel_during_release_does_not_leak_ticket(self):
        # cancel_audio 紧跟断线会对同一个 ticket 取消两次。release() 在 condition
        # 上挂起时被取消，票就永久留在 active——独占模式下之后每次听写都卡死。
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        ticket = await scheduler.acquire("a", "asr", 100, "exclusive")

        # 让 release 里的 condition 被别人占着，制造挂起窗口。
        blocker = asyncio.Event()

        async def hog():
            async with scheduler.condition:
                await blocker.wait()

        hog_task = asyncio.create_task(hog())
        await asyncio.sleep(0.02)

        releasing = asyncio.create_task(ticket.release())
        await asyncio.sleep(0.02)
        releasing.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await releasing

        blocker.set()
        await hog_task
        await asyncio.sleep(0.05)

        self.assertEqual(scheduler.status()["reserved_mb"], 0)
        # 独占票没漏掉，后面的任务才能拿到票。
        nxt = await asyncio.wait_for(scheduler.acquire("b", "asr", 100, "exclusive"), 2)
        await nxt.release()

    async def test_expired_ticket_is_evicted(self):
        # 持有者线程卡死在驱动里、票没还，租约到期必须强制回收，
        # 否则独占票会跨重连一直废掉整个引擎。
        scheduler = GpuScheduler(FakeProvider())
        scheduler.reserve_percent = lambda: 15
        scheduler.LEASE_S = 0.05
        leaked = await scheduler.acquire("stuck", "asr", 100, "exclusive")
        self.assertEqual(scheduler.status()["active"], 1)
        await asyncio.sleep(0.1)
        ticket = await asyncio.wait_for(scheduler.acquire("b", "refine", 100, "exclusive"), 2)
        await ticket.release()
        self.assertEqual(scheduler.status()["reserved_mb"], 0)
        del leaked


class EstimateTests(unittest.TestCase):
    def test_model_mb_sums_directory_weights(self):
        # HF 后端给的是目录；对目录本身 stat 只有几 KB，权重必须逐个文件累加。
        from engine import _model_mb
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root / "model.safetensors").write_bytes(b"\0" * (4 * 1024 * 1024))
            (root / "tokenizer.json").write_text("{}")
            self.assertAlmostEqual(_model_mb(root), 4.0, places=1)

    def test_estimate_scales_with_weights(self):
        # 权重要算进增量，不能只留 workspace——低估会把该排队的阶段放进来。
        from engine import stage_estimate_mb
        with patch("engine._model_mb", return_value=3888.0):      # qwen3-asr-1.7b
            self.assertGreater(stage_estimate_mb("asr", Path("x")), 4000)
        with patch("engine._model_mb", return_value=4900.0):      # qwen3-8b gguf
            self.assertGreater(stage_estimate_mb("llm", Path("x")), 6000)

    def test_missing_model_falls_back(self):
        from engine import stage_estimate_mb
        self.assertEqual(stage_estimate_mb("asr", Path("nope")), 900)
        self.assertEqual(stage_estimate_mb("llm", Path("nope")), 1536)


class FakeSocket:
    remote_address = ("test", 1)

    def __init__(self, messages, finish_delay=0):
        self.messages = messages
        self.finish_delay = finish_delay
        self.sent = []

    def __aiter__(self):
        self._messages = iter(self.messages)
        self._delayed = False
        return self

    async def __anext__(self):
        try:
            message = next(self._messages)
            await asyncio.sleep(0)
            return message
        except StopIteration:
            if self.finish_delay and not self._delayed:
                self._delayed = True
                await asyncio.sleep(self.finish_delay)
            raise StopAsyncIteration

    async def send_str(self, payload):
        self.sent.append(json.loads(payload))


class FakeSession:
    finalized = []
    cancelled = []

    def __init__(self, _ws, audio_id, _asr, _llm, _scheduler, send_json):
        self.audio_id = audio_id
        self.send_json = send_json
        self.chunks = []
        self.started = False

    async def on_start(self, _data):
        return None

    async def on_chunk(self, payload):
        self.chunks.append(payload)

    async def on_end(self):
        self.started = True
        try:
            delay = {"a": 0.15, "b": 0.02, "c": 0.08}[self.audio_id]
            await asyncio.sleep(delay)
            self.finalized.append((self.audio_id, b"".join(self.chunks)))
            await self.send_json({"type": "refine_completed", "audio_id": self.audio_id})
        except asyncio.CancelledError:
            self.cancelled.append(self.audio_id)
            raise


class HandleTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        FakeSession.finalized = []
        FakeSession.cancelled = []

    async def test_three_tagged_sessions_route_and_finish_out_of_order(self):
        messages = []
        for audio_id in ("a", "b", "c"):
            messages.extend([
                json.dumps({"type": "start_audio", "audio_id": audio_id}),
                encode_audio_frame(audio_id, audio_id.encode()),
                json.dumps({"type": "end_audio", "audio_id": audio_id}),
            ])
        socket = FakeSocket(messages, finish_delay=0.2)
        scheduler = GpuScheduler(FakeProvider())
        with patch("engine.Session", FakeSession):
            await handle(socket, object(), object(), scheduler)

        completed = [item["audio_id"] for item in socket.sent if item["type"] == "refine_completed"]
        self.assertEqual(completed, ["b", "c", "a"])
        self.assertCountEqual(FakeSession.finalized, [("a", b"a"), ("b", b"b"), ("c", b"c")])
        self.assertEqual(scheduler.status()["reserved_mb"], 0)

    async def test_ambiguous_untagged_frame_is_rejected(self):
        socket = FakeSocket([
            json.dumps({"type": "start_audio", "audio_id": "a"}),
            json.dumps({"type": "start_audio", "audio_id": "b"}),
            b"RIFFuntagged",
        ])
        with patch("engine.Session", FakeSession):
            await handle(socket, object(), object(), GpuScheduler(FakeProvider()))

        errors = [item for item in socket.sent if item["type"] == "protocol_error"]
        self.assertEqual(errors[0]["error"], "untagged audio is ambiguous")
        self.assertEqual(FakeSession.finalized, [])

    async def test_disconnect_cancels_running_finalizer(self):
        socket = FakeSocket([
            json.dumps({"type": "start_audio", "audio_id": "a"}),
            encode_audio_frame("a", b"a"),
            json.dumps({"type": "end_audio", "audio_id": "a"}),
        ], finish_delay=0.01)
        with patch("engine.Session", FakeSession):
            await handle(socket, object(), object(), GpuScheduler(FakeProvider()))

        self.assertEqual(FakeSession.cancelled, ["a"])


class StubAsr:
    """记下每次 transcribe 拿到多少音频和多大的上下文预算。"""
    backend = "transformers-qwen-asr"
    path = None

    def __init__(self, lang="zh"):
        self.seen_samples = []
        self.seen_ctx_limits = []
        self.lang = lang

    async def transcribe(self, pcm, initial_prompt=None, lang=None,
                         qwen_ctx_max_chars=None):
        self.seen_samples.append(len(pcm))
        self.seen_ctx_limits.append(qwen_ctx_max_chars)
        return f"seg{len(self.seen_samples)}", self.lang


class CpuOnlyTests(unittest.IsolatedAsyncioTestCase):
    async def test_cpu_mode_admits_even_with_no_free_vram(self):
        """显卡被 ComfyUI 占满时的场景：一点空闲显存都没有。CPU 模式下权重根本
        不进显存，再按显存排队就是永远等不到的自我阻塞。"""
        scheduler = GpuScheduler(FakeProvider(total_mb=16000, free_mb=0))
        with patch("engine.cpu_only", return_value=True):
            ticket = await asyncio.wait_for(
                scheduler.acquire("job", "refine", 6000), timeout=2)
            await ticket.release()

    async def test_gpu_mode_still_queues_when_vram_is_short(self):
        """CPU 开关关掉时，原来的显存准入必须一字不动。"""
        scheduler = GpuScheduler(FakeProvider(total_mb=16000, free_mb=0))
        scheduler.reserve_percent = lambda: 15
        first = await scheduler.acquire("a", "refine", 6000)   # 独票逃生放行
        with patch("engine.cpu_only", return_value=False):
            with self.assertRaises(asyncio.TimeoutError):
                await asyncio.wait_for(
                    asyncio.shield(scheduler.acquire("b", "refine", 6000)), timeout=0.3)
        await first.release()

    async def test_cpu_ticket_is_released_normally(self):
        scheduler = GpuScheduler(FakeProvider(total_mb=16000, free_mb=0))
        with patch("engine.cpu_only", return_value=True):
            ticket = await scheduler.acquire("job", "asr", 5000)
            self.assertTrue(scheduler.active)
            await ticket.release()
        self.assertFalse(scheduler.active, "CPU 票没回收，租约到期前会一直占着位置")


class LearningTests(unittest.IsolatedAsyncioTestCase):
    async def test_learning_uses_the_dedicated_model(self):
        """精修链路删掉之后，学词是全仓库唯一还需要 LLM 的事，模型由它自己挑。"""
        import engine
        asked = []

        def fake_pick(st):
            asked.append(st.get("learnModel"))
            return Path("learn.gguf")

        class Llm:
            async def chat(self, *a, **k):
                return '{"add": true, "term": "Localless", "tag": "编程项目"}'

        sent = []
        with patch("engine.learn_model_path", fake_pick), \
             patch("engine.stage_estimate_mb", return_value=10):
            await engine.review_learning(
                GpuScheduler(FakeProvider()), Llm(), {"learnModel": "qwen3-4b"},
                {"request_id": "r1"}, "cloud code", "Localless",
                lambda d: sent.append(d) or asyncio.sleep(0))

        self.assertEqual(asked, ["qwen3-4b"], "学词没有读设置里指定的模型")
        self.assertTrue(sent and sent[0]["add"])
        self.assertEqual(sent[0]["term"], "Localless")

    async def test_learning_always_replies_even_when_the_model_fails(self):
        """判定失败也必须回一条，否则客户端那边的观察器永远收不到结果。"""
        import engine

        class Broken:
            async def chat(self, *a, **k):
                raise RuntimeError("模型炸了")

        sent = []
        with patch("engine.learn_model_path", return_value=Path("learn.gguf")), \
             patch("engine.stage_estimate_mb", return_value=10):
            await engine.review_learning(
                GpuScheduler(FakeProvider()), Broken(), {}, {"request_id": "r2"},
                "a", "b", lambda d: sent.append(d) or asyncio.sleep(0))
        self.assertTrue(sent, "模型失败时一条回复都没发")
        self.assertFalse(sent[0]["add"])


class FocusedContextTests(unittest.TestCase):
    """开关删了之后，上下文无条件送进来，用不用由模板里写不写 {{focused}} 决定。
    这里钉的是「确实送到了」——prompt_vars 里那一行被误删的话没有别的测试会响。"""

    def test_input_box_text_always_reaches_the_model(self):
        from engine import prompt_vars
        self.assertEqual(prompt_vars({}, focused_text="框里已有")["focused"], "框里已有")

    def test_context_is_budgeted_not_switched(self):
        # 截断不是开关是预算：上下文再有用也不能把提示词顶过长度上限（那会让 ASR 吐空）
        from engine import prompt_vars
        v = prompt_vars({}, focused_text="x" * 900, history=["y" * 400])
        self.assertEqual(len(v["focused"]), 500)
        self.assertEqual(len(v["history"]), 300)


class EchoStripTests(unittest.TestCase):
    """输入框里已有的字被顺口念了一遍当引子，粘回去就成了"我想把我想把…"。
    交给 4B 判只对一半（还会照抄示例里的字），所以精确重叠用代码删。"""

    def test_strips_the_repeated_lead_in(self):
        from engine import strip_focused_echo as f
        self.assertEqual(f("我想把", "我想把这段话改得更短一点"), "这段话改得更短一点")
        self.assertEqual(f("帮我查一下", "帮我查一下明天上海的天气"), "明天上海的天气")
        self.assertEqual(f("abc", "abcdef"), "def")

    def test_leaves_unrelated_text_alone(self):
        from engine import strip_focused_echo as f
        self.assertEqual(f("今天的会议记录：", "下午三点在二楼会议室"), "下午三点在二楼会议室")
        self.assertEqual(f("", "这个方案我觉得可以"), "这个方案我觉得可以")

    def test_one_character_overlap_is_a_coincidence(self):
        from engine import strip_focused_echo as f
        # "结尾是的" 和 "的确不错" 只重叠一个"的"，删掉就把用户的话吃了
        self.assertEqual(f("结尾是的", "的确不错"), "的确不错")

    def test_never_returns_empty(self):
        from engine import strip_focused_echo as f
        # 整句都跟输入框重复时，宁可留着重复也不能输出空
        self.assertEqual(f("我想把", "我想把"), "我想把")


class ExistingWordsTests(unittest.TestCase):
    """词表里已经有的词不该再被"学"一次。客户端会兜底去重，但模型不知情就会每次
    白跑一趟判定、再弹一次"已添加"，用户以为又学到了新词。"""

    ST = {"customWords": [{"text": "GitHub"}, {"text": "示例关卡项目"}],
          "customWordsEnabled": True}

    def test_existing_words_are_listed_for_the_model(self):
        from engine import build_learn_prompt
        out = build_learn_prompt(self.ST, "a", "b")
        # 默认模板走 {{words}}；老的自定义模板走自动追加。两条路都得让模型看到词表。
        self.assertIn("示例关卡项目", out)
        self.assertIn("GitHub", out)

    def test_fallback_rule_sits_before_the_content(self):
        """用户的旧模板没有 {{words}} 时才会自动追加。那段规则必须排在
        改之前/改之后 之前，否则模型会把它当成待判定的内容。"""
        from engine import build_learn_prompt
        st = {**self.ST, "learnPrompt": "判断一下。\n改之前：{{wrong}}\n改之后：{{right}}"}
        out = build_learn_prompt(st, "a", "b")
        self.assertIn("已经在词表里了", out, "旧模板没有自动补上词表")
        self.assertLess(out.index("已经在词表里了"), out.index("改之前"),
                        "规则排在 改之前/改之后 后面，模型会把它当成待判定的内容")

    def test_no_rule_when_the_vocabulary_is_empty(self):
        from engine import build_learn_prompt
        out = build_learn_prompt({}, "a", "b")
        self.assertNotIn("已经在词表里了", out)

    def test_disabled_vocabulary_lists_nothing(self):
        from engine import existing_words_rule
        self.assertEqual(existing_words_rule({**self.ST, "customWordsEnabled": False}), "")

    def test_template_using_words_variable_wins(self):
        """模板里写了 {{words}} 就说明用户自己安排了词表的位置和措辞，
        这时候再硬塞一段看不见也改不了的规则就是越权。"""
        from engine import existing_words_rule
        self.assertEqual(existing_words_rule(self.ST, "已有词：{{words}}\n改之前：{{wrong}}"), "")
        self.assertEqual(existing_words_rule(self.ST, "{{wordsByTag}}"), "")
        self.assertNotEqual(existing_words_rule(self.ST, "改之前：{{wrong}}"), "",
                            "模板没提词表时必须自动补上，否则模型完全不知道已有什么")


class DefaultPromptSyncTests(unittest.TestCase):
    """默认提示词以前有两份：engine.py 用来渲染，settings.html 用来"恢复默认"。
    两份各改各的就出现"我在编辑器里看不到这个变量"——{{words}} 那次就是只加了引擎
    那份。现在唯一真源是 app/prompts.json，两边都读它；这些测试盯着别再抄回去。"""

    def _settings_src(self):
        # 设置页搬到 tauri/src/ 去了（Electron 版删干净那次）。engine.py 还留在
        # app/ 下，两边隔着一层目录，所以这里必须往上走一级再拐进 tauri/src。
        root = Path(__file__).resolve().parent.parent
        return (root / "tauri" / "src" / "settings.html").read_text(encoding="utf-8")

    def test_settings_page_reads_the_shared_file(self):
        src = self._settings_src()
        self.assertIn("prompts.json", src, "设置页没有读 prompts.json")
        self.assertIn("PROMPT_FILE.defaults", src, "设置页没把默认值接到共享文件上")

    def test_settings_page_no_longer_hardcodes_defaults(self):
        import re as _re
        src = self._settings_src()
        for key in ("refinePrompt", "learnPrompt", "qualityGatePrompt", "thinkingPrompt"):
            self.assertIsNone(_re.search(key + r":\s*'", src),
                              f"settings.html 又硬编码了一份 {key} 默认值——两份副本迟早跑偏")

    def test_shared_file_is_the_source_engine_uses(self):
        import json as _json
        from engine import DEFAULT_PROMPTS, PROMPTS_PATH
        data = _json.loads(PROMPTS_PATH.read_text(encoding="utf-8"))
        self.assertEqual(data["defaults"], DEFAULT_PROMPTS,
                         "引擎的默认值和 prompts.json 对不上")

    def test_learn_prompt_exposes_the_words_variable(self):
        from engine import DEFAULT_PROMPTS
        self.assertIn("{{words}}", DEFAULT_PROMPTS["learnPrompt"],
                      "默认模板里没有 {{words}}，模型看不到已有词表，用户也看不见")

    def test_words_variable_actually_renders_the_vocabulary(self):
        from engine import build_learn_prompt
        st = {"customWords": [{"text": "示例关卡项目"}], "customWordsEnabled": True}
        out = build_learn_prompt(st, "a", "b")
        self.assertIn("示例关卡项目", out)
        # 模板自己引用了 {{words}}，就不该再追加那段看不见的规则
        self.assertNotIn("补充规则", out, "模板已经有 {{words}} 了还在硬塞规则")


class PromptMigrationTests(unittest.TestCase):
    """保存值等于旧默认 = 用户从没自定义过，可以安全升级。真改过的一个字都不动。"""

    def test_untouched_old_default_is_dropped(self):
        from engine import migrate_prompts, LEGACY_PROMPTS
        old = LEGACY_PROMPTS["asrPrompt"][0]
        st = migrate_prompts({"asrPrompt": old})
        self.assertNotIn("asrPrompt", st,
                         "旧默认没被清掉，用户永远拿不到新提示词")

    def test_old_default_plus_text_slot_still_counts_as_untouched(self):
        # 槽位是占位不是措辞：只多了个 {{text}} 不算用户改过
        from engine import migrate_prompts, LEGACY_PROMPTS
        old = LEGACY_PROMPTS["asrPrompt"][0]
        st = migrate_prompts({"asrPrompt": old + "\n转写原文：\n{{text}}"})
        self.assertNotIn("asrPrompt", st, "只多了个占位槽就被当成自定义了")

    def test_user_edits_are_never_touched(self):
        from engine import migrate_prompts, LEGACY_PROMPTS
        mine = LEGACY_PROMPTS["asrPrompt"][0] + "\n我自己加的一句。"
        st = migrate_prompts({"asrPrompt": mine})
        self.assertEqual(st["asrPrompt"], mine, "覆盖了用户自己改过的提示词")

    def test_migrated_settings_render_with_the_new_default(self):
        from engine import migrate_prompts, LEGACY_PROMPTS, asr_prompt
        st = migrate_prompts({"asrPrompt": LEGACY_PROMPTS["asrPrompt"][0],
                              "customWords": [{"text": "Localless"}],
                              "customWordsEnabled": True})
        self.assertIn("阿拉伯数字", asr_prompt(st, ""),
                      "迁移完还是旧模板，新默认里的数字规则没生效")



class QwenContextLimitTests(unittest.TestCase):
    """Qwen3-ASR 长上下文是可调的可靠性预算，不是固定模型硬上限。"""

    def test_default_budget_remains_conservative(self):
        from engine import QWEN_CTX_MAX_CHARS
        self.assertEqual(QWEN_CTX_MAX_CHARS, 480)

    def test_budget_validation_and_boundaries(self):
        from engine import qwen_ctx_limit
        self.assertEqual(qwen_ctx_limit(None), 480)
        self.assertEqual(qwen_ctx_limit("bad"), 480)
        self.assertEqual(qwen_ctx_limit(40), 100)
        self.assertEqual(qwen_ctx_limit(2600), 2600)
        self.assertEqual(qwen_ctx_limit(9000), 4000)

    def test_explicit_budget_controls_clipping(self):
        from engine import clip_qwen_ctx
        ctx = ", ".join(f"词{i:04d}" for i in range(800))
        short = clip_qwen_ctx(ctx, 200)
        long = clip_qwen_ctx(ctx, 1600)
        self.assertLessEqual(len(short), 200)
        self.assertLessEqual(len(long), 1600)
        self.assertGreater(len(long), len(short) * 4)

    def test_long_context_is_clipped(self):
        from engine import clip_qwen_ctx, QWEN_CTX_MAX_CHARS
        self.assertLessEqual(len(clip_qwen_ctx("词, " * 500)), QWEN_CTX_MAX_CHARS)

    def test_short_context_is_untouched(self):
        from engine import clip_qwen_ctx
        self.assertEqual(clip_qwen_ctx("人名: Alice"), "人名: Alice")
        self.assertIsNone(clip_qwen_ctx(None))

    def test_clip_cuts_on_a_separator(self):
        from engine import clip_qwen_ctx
        out = clip_qwen_ctx("人名: " + ", ".join(f"词{i:03d}" for i in range(300)))
        self.assertFalse(out.endswith(","), "截在了分隔符上，留了个半截")
        self.assertTrue(out.endswith(tuple("0123456789")), "把词切成半个了")


class AudioProcessorTests(unittest.TestCase):
    def test_disabled_zero_db_path_is_transparent(self):
        import numpy as np
        from engine import AudioProcessor
        x = np.random.default_rng(4).uniform(-0.8, 0.8, 4096).astype(np.float32)
        np.testing.assert_array_equal(AudioProcessor(0, False).process(x), x)

    def test_gain_changes_amplitude(self):
        import numpy as np
        from engine import AudioProcessor
        x = np.full(1024, 0.02, np.float32)
        out = AudioProcessor(20, False).process(x)
        self.assertAlmostEqual(float(np.sqrt(np.mean(out ** 2))), 0.2, places=3)

    def test_process_does_not_clamp(self):
        """process() 故意不限幅——整段峰值要等录完才知道，见 normalize 的长注释。

        这条以前断言的是 process() 的输出 ≤ 1.0，也就是「限幅在流式那一段」。
        那正是 rec-beb1 丢掉整段音频的成因，限幅已经挪走了。留着反向断言，
        是防止有人图省事把它挪回来。
        """
        import numpy as np
        from engine import AudioProcessor
        out = AudioProcessor(30, False).process(np.full(1000, 0.9, np.float32))
        self.assertGreater(float(np.max(out)), 1.0, "限幅又回到流式那一段了")

    def test_normalize_stays_bounded(self):
        import numpy as np
        from engine import AudioProcessor
        p = AudioProcessor(30, False)
        out = p.normalize(p.process(np.full(1000, 0.9, np.float32)))
        self.assertLessEqual(float(np.max(np.abs(out))), 1.0)
        # 超标的整段按峰值缩到 0.95，纯标量，不带失真。
        self.assertAlmostEqual(float(np.max(out)), 0.95, places=4)

    def test_normalize_leaves_quiet_audio_alone(self):
        """安静的麦这条路要和限幅挪走之前一字不差：峰值没过 0.95 就原样返回。"""
        import numpy as np
        from engine import AudioProcessor
        x = np.full(1000, 0.12, np.float32)
        np.testing.assert_array_equal(AudioProcessor(0, False).normalize(x), x)

    def test_high_pass_attenuates_low_frequency_more_than_voice_band(self):
        import numpy as np
        from engine import AudioProcessor, SAMPLE_RATE
        t = np.arange(SAMPLE_RATE, dtype=np.float32) / SAMPLE_RATE
        low = 0.1 * np.sin(2 * np.pi * 20 * t)
        voice = 0.1 * np.sin(2 * np.pi * 1000 * t)
        low_out = AudioProcessor(0, True)._high_pass(low)
        voice_out = AudioProcessor(0, True)._high_pass(voice)
        low_ratio = np.sqrt(np.mean(low_out[1000:] ** 2)) / np.sqrt(np.mean(low[1000:] ** 2))
        voice_ratio = np.sqrt(np.mean(voice_out[1000:] ** 2)) / np.sqrt(np.mean(voice[1000:] ** 2))
        self.assertLess(low_ratio, voice_ratio * 0.5)

    def test_gate_suppresses_stable_noise_but_preserves_voice(self):
        import numpy as np
        from engine import AudioProcessor
        p = AudioProcessor(0, True)
        noise = np.random.default_rng(8).normal(0, 0.0004, 16000).astype(np.float32)
        noise_out = p.process(noise)
        voice = np.random.default_rng(9).normal(0, 0.03, 16000).astype(np.float32)
        voice_out = p.process(voice)
        self.assertLess(np.sqrt(np.mean(noise_out[-8000:] ** 2)),
                        np.sqrt(np.mean(noise[-8000:] ** 2)) * 0.2)
        self.assertGreater(np.sqrt(np.mean(voice_out[-8000:] ** 2)),
                           np.sqrt(np.mean(voice[-8000:] ** 2)) * 0.8)

    def test_gain_is_applied_before_the_gate(self):
        import numpy as np
        from engine import AudioProcessor
        # 一支安静的实体麦：原始电平卡在门的绝对下限 0.0015 以下，光靠自己开不了门。
        # 用户手里唯一的旋钮是增益——增益加在门之后时，这个旋钮对门毫无作用，整段
        # 被压成 0.06 倍（rec-8bf0 就是这么无声的）；加在门之前，门就开得了。
        quiet = np.random.default_rng(31).normal(0, 0.0008, 16000).astype(np.float32)
        out = AudioProcessor(12, True).process(quiet)
        rms = lambda a: float(np.sqrt(np.mean(a[-8000:] ** 2)))
        self.assertGreater(rms(out), rms(quiet) * 10 ** (12 / 20) * 0.5)

    def test_floor_does_not_learn_before_the_gate_ever_opens(self):
        import numpy as np
        from engine import AudioProcessor
        p = AudioProcessor(0, True)
        rng = np.random.default_rng(32)
        # 两秒刚好卡在门槛下的声音。门一次没开过，这东西是底噪还是被误判的人声无从
        # 分辨——学进去，门槛就被自己抬上去，后面真说话也再开不了。
        for _ in range(20):
            p.process(rng.normal(0, 0.0013, 1600).astype(np.float32))
        self.assertAlmostEqual(p.noise_floor, 0.0005, places=6)
        voice = rng.normal(0, 0.01, 16000).astype(np.float32)
        out = p.process(voice)
        self.assertGreater(float(np.sqrt(np.mean(out[-8000:] ** 2))),
                           float(np.sqrt(np.mean(voice[-8000:] ** 2))) * 0.8)

    def test_high_pass_state_is_continuous_across_chunks(self):
        import numpy as np
        from engine import AudioProcessor
        x = np.random.default_rng(12).normal(0, 0.05, 4096).astype(np.float32)
        whole = AudioProcessor(0, True)._high_pass(x)
        chunked_processor = AudioProcessor(0, True)
        chunked = np.concatenate((chunked_processor._high_pass(x[:1377]),
                                  chunked_processor._high_pass(x[1377:])))
        np.testing.assert_allclose(chunked, whole, atol=1e-7)


class SessionAudioProcessingTests(unittest.IsolatedAsyncioTestCase):
    async def test_stored_audio_and_vad_receive_the_same_processed_signal(self):
        import numpy as np
        from engine import Session, AudioProcessor, SAMPLE_RATE

        class CaptureVad:
            def __init__(self):
                self.frames = []

            def prob(self, frame):
                self.frames.append(frame.copy())
                return 0.0

        session = Session(object(), "audio-dsp", StubAsr(), object(),
                          GpuScheduler(FakeProvider()))
        session.meta = {"audio_format": "pcm", "audio_sample_rate": SAMPLE_RATE,
                        "audio_channels": 1}
        session.audio_processor = AudioProcessor(6, False)
        session.vad = CaptureVad()
        source = np.linspace(-0.1, 0.1, 1024, dtype=np.float32)
        payload = (source * 32767).astype(np.int16).tobytes()
        await session.on_chunk(payload)
        stored = session.audio_f32()
        seen = np.concatenate(session.vad.frames)
        np.testing.assert_allclose(seen, stored[:len(seen)], atol=1 / 32767)


class SessionSettingsSnapshotTests(unittest.IsolatedAsyncioTestCase):
    async def test_context_and_audio_settings_are_frozen_at_start(self):
        from engine import Session

        class StartAsr(StubAsr):
            def pending_change(self):
                return False

        first = {"qwenCtxMaxChars": 1800, "micGainDb": 9,
                 "noiseReductionEnabled": True}
        session = Session(object(), "snapshot", StartAsr(), object(),
                          GpuScheduler(FakeProvider()),
                          send_json=lambda data: asyncio.sleep(0))
        with patch("engine.load_settings", return_value=first):
            await session.on_start({"audio_metadata": {"audio_format": "pcm"}})
        self.assertEqual(session.qwen_ctx_max_chars, 1800)
        self.assertEqual(session.audio_processor.gain_db, 9)
        self.assertTrue(session.audio_processor.noise_reduction)

        first.update(qwenCtxMaxChars=300, micGainDb=-12,
                     noiseReductionEnabled=False)
        self.assertEqual(session.qwen_ctx_max_chars, 1800)
        self.assertEqual(session.audio_processor.gain_db, 9)
        self.assertTrue(session.audio_processor.noise_reduction)

    async def test_invalid_snapshot_values_use_safe_defaults(self):
        from engine import Session

        class StartAsr(StubAsr):
            def pending_change(self):
                return False

        session = Session(object(), "snapshot-defaults", StartAsr(), object(),
                          GpuScheduler(FakeProvider()),
                          send_json=lambda data: asyncio.sleep(0))
        with patch("engine.load_settings", return_value={
                "qwenCtxMaxChars": "broken", "micGainDb": float("nan")}):
            await session.on_start({})
        self.assertEqual(session.qwen_ctx_max_chars, 480)
        self.assertEqual(session.audio_processor.gain_db, 0)


class HallucinationTests(unittest.TestCase):
    """样本照着 engine.log 里真实出现过的 ASR 输出的样子编的。"""

    def test_strips_repeated_filler_symbols(self):
        from engine import _dehallucinate
        raw = "这样应该就可以了．" + "﹏" * 75
        self.assertEqual(_dehallucinate(raw), "这样应该就可以了．")

    def test_collapses_whole_phrase_duplication(self):
        from engine import _dehallucinate
        self.assertEqual(_dehallucinate("好了,我再说一遍？好了,我再说一遍？"),
                         "好了,我再说一遍？")

    def test_drops_youtube_subtitle_tail(self):
        from engine import _dehallucinate
        raw = "它会多出来一些没说过的词． 感谢观看"
        self.assertEqual(_dehallucinate(raw), "它会多出来一些没说过的词．")

    def test_drops_a_transcript_that_is_only_hallucination(self):
        from engine import _dehallucinate
        self.assertEqual(_dehallucinate("感谢观看"), "")
        self.assertEqual(_dehallucinate("Thanks for watching!"), "")

    def test_keeps_real_speech_untouched(self):
        from engine import _dehallucinate
        for good in ("升级到 2.0 版对工作流有什么影响？",
                     "我今天想说的话是这样的。",
                     "他说了又说，说了又说，这是真的重复表达"):
            self.assertEqual(_dehallucinate(good), good, f"误伤了正常句子：{good}")


if __name__ == "__main__":
    unittest.main()
