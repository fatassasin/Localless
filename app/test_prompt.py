"""最小自检：词语分组 + 提示词编排。python test_prompt.py"""
from engine import words_by_tag, setting_words

ST = {
    "customWordsEnabled": True,
    "wordTags": [{"id": "people", "name": "人名"}, {"id": "proj", "name": "编程项目"}],
    "customWords": [
        {"text": "张三", "tag": "people"},
        {"text": "Localless", "tag": "proj"},
        {"text": "Blender", "tag": "proj"},
    ],
}

t = words_by_tag(ST)
assert "人名: 张三" in t and "编程项目: Localless, Blender" in t, t
assert words_by_tag({**ST, "customWordsEnabled": False}) == ""
assert setting_words(ST) == ["张三", "Localless", "Blender"]

# ---- ASR 提示词 ----
from engine import asr_prompt

# 有词表就该发
wp = asr_prompt(ST, "")
assert wp and "张三" in wp and "Localless" in wp, wp
# 只有输入框内容、没有词表，也值得发一次：术语和语气都靠它对齐
assert asr_prompt({"customWords": [], "customWordsEnabled": True}, "上文在这") is not None
# 两样都没有才回 None——空提示词只会白白占掉 ASR 的上下文预算
assert asr_prompt({"customWords": [], "customWordsEnabled": True}, "") is None

# 分类/偏置一律不许把程序名塞给模型：同一个浏览器里既可能在写邮件也可能在写代码，
# 进程名说明不了任何事，但只要写进提示词，模型就会拿它当依据。
assert "当前程序" not in wp, wp

# ASR 提示词不许再吃 corrections。以前它把每条手动修改的 right（改后的**整句**）
# 当热词塞进词表，用户打开提示词看到的是一堆自己以前说过的话，词表反被挤掉。
# 参数直接删掉才挡得住——留着不用，下次总会有人再接回去。
import inspect
assert "corrections" not in inspect.signature(asr_prompt).parameters, \
    "asr_prompt 又长回 corrections 参数了：整句会被当热词塞进 ASR 上下文"

# 编排：转写原文必须收尾。模板中间写了 {{text}} 也要挪到末尾，历史/输入框上下文排它前面。
from engine import build_prompt, VAR_LABELS
P = build_prompt("规则\n转写原文：\n{{text}}\n更多规则",
                 {"text": "这是转写", "focused": "框里已有", "history": "上一条说过"})
assert P.rstrip().endswith("这是转写"), P
assert P.count("转写原文：") == 1, P                 # 模板自带那句被吃掉，只剩追加的
assert P.index("上一条说过") < P.index("这是转写"), P
assert P.index("框里已有") < P.index("这是转写"), P
assert "更多规则" in P, P
# 空 text 不该凭空追加标签
assert "转写原文" not in build_prompt("规则", {"text": "", "focused": "", "history": ""})

print("ok")
