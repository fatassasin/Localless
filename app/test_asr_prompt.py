"""Qwen3-ASR 上下文注入的测试。

单独成文件是因为要加载 transformers processor，比 test_parallel 慢一个量级；
模型不在就整体跳过，不拖累快测。

这里每一条都是冲着同一个 bug 去的：原来 `apply_transcription_request(..., prompt=ctx)`
里的 `prompt` 根本不是那个函数的参数，transformers 打一行警告就丢掉，词表从未进过模型。
纯逻辑测试抓不到——调用不报错、返回值形状也全对，只是内容少了一段。能抓到它的只有
"带词表和不带词表，喂进模型的 token 必须不一样"这一条。
"""
import unittest
import numpy as np

import engine

MODEL = engine.catalog_path(engine.model_catalog().get("qwen3-asr-1.7b-hf") or {})
HAVE = bool(MODEL) and MODEL.exists()


@unittest.skipUnless(HAVE, "qwen3-asr-1.7b-hf 不在本机，跳过")
class QwenAsrPromptTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        from transformers import AutoProcessor
        cls.proc = AutoProcessor.from_pretrained(str(MODEL), local_files_only=True)
        cls.audio = (np.random.default_rng(0).standard_normal(16000) * 0.01).astype(np.float32)
        cls.ctx = "人名: Alice, 张三; 编程项目: Blender, Localless"

    def _asr(self):
        a = object.__new__(engine.Asr)      # 只测输入组装，不加载 1.7B 权重
        a.processor = self.proc
        return a

    def _decode(self, inputs):
        return self.proc.tokenizer.decode(inputs["input_ids"][0])

    def _system_of(self, inputs):
        return self._decode(inputs).split("<|audio_start|>")[0]

    def test_context_actually_changes_what_the_model_sees(self):
        a = self._asr()
        without = a._qwen_inputs(self.audio, None, "zh")
        with_ctx = a._qwen_inputs(self.audio, self.ctx, "zh")
        self.assertGreater(with_ctx["input_ids"].shape[1], without["input_ids"].shape[1],
                           "带词表和不带词表喂进模型的 token 一样长——词表又被丢掉了")

    def test_vocabulary_lands_in_the_system_turn(self):
        sys_seg = self._system_of(self._asr()._qwen_inputs(self.audio, self.ctx, "zh"))
        self.assertIn("张三", sys_seg, "词表没进 system 段")
        self.assertIn("Blender", sys_seg)

    def test_language_hint_survives_alongside_the_context(self):
        sys_seg = self._system_of(self._asr()._qwen_inputs(self.audio, self.ctx, "zh"))
        self.assertIn("Chinese", sys_seg, "加了词表就把语言提示挤掉了")
        # 模板把多条 system 直接拼接，不补换行就会粘成「Chinese人名: ...」
        self.assertNotIn("Chinese人名", sys_seg, "语言名和词表粘在一起了，少了分隔")

    def test_audio_features_match_the_builtin_path(self):
        a = self._asr()
        ref = self.proc.apply_transcription_request(audio=self.audio, language="zh")
        mine = a._qwen_inputs(self.audio, self.ctx, "zh")
        self.assertEqual(sorted(ref.keys()), sorted(mine.keys()), "张量字段和内置路径对不上")
        self.assertEqual(ref["input_features"].shape, mine["input_features"].shape)
        self.assertEqual(ref["input_features_mask"].shape, mine["input_features_mask"].shape)

    def test_no_language_still_works(self):
        # 用户开着自动检测时 whisper_lang() 返回 None，这是最常走的一条路
        sys_seg = self._system_of(self._asr()._qwen_inputs(self.audio, self.ctx, None))
        self.assertIn("张三", sys_seg, "自动检测模式下词表丢了")

    def test_language_codes_map_to_full_names(self):
        a = self._asr()
        self.assertEqual(a._qwen_lang_name("zh"), "Chinese")
        self.assertEqual(a._qwen_lang_name("en"), "English")
        self.assertIsNone(a._qwen_lang_name(None))

    def test_explicit_context_budget_changes_system_length(self):
        a = self._asr()
        ctx = ", ".join(f"专名{i:04d}" for i in range(700))
        short = self._system_of(a._qwen_inputs(self.audio, ctx, "zh", 200))
        long = self._system_of(a._qwen_inputs(self.audio, ctx, "zh", 1600))
        self.assertGreater(len(long), len(short) * 4,
                           "显式上下文预算没有改变实际送进 system 段的长度")

    def test_unknown_language_code_never_raises(self):
        # whisper_lang() 会把 zh-Hans 削成 zh，但设置文件是用户能手改的，别让它炸掉听写
        a = self._asr()
        for bad in ("zh-Hans", "klingon", "xx-YY"):
            with self.subTest(lang=bad):
                got = a._qwen_lang_name(bad)      # 不抛异常就算过
                self.assertIn(type(got), (str, type(None)))
        # zh-Hans 虽然 processor 不认，但兜底表按主码认得出来
        self.assertEqual(a._qwen_lang_name("zh-Hans"), "Chinese")

    def test_context_free_call_matches_builtin_token_count(self):
        a = self._asr()
        ref = self.proc.apply_transcription_request(audio=self.audio, language="zh")
        mine = a._qwen_inputs(self.audio, None, "zh")
        self.assertEqual(ref["input_ids"].shape[1], mine["input_ids"].shape[1],
                         "没有词表时应该和内置路径完全等价")


if __name__ == "__main__":
    unittest.main()
