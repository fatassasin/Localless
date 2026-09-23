# -Serve: stay alive and answer one JSON line per line of stdin, instead of
# doing one read and exiting.
#
# Why this mode exists: a cold `powershell -File uia-helper.ps1` costs ~335ms on
# this machine, and almost none of that is the UIA query -- it is process start
# plus Add-Type. Delivery reads the focused control up to four times per
# dictation (two paste shots x two confirmations), so the cold starts alone were
# ~1.3s of the ~2.4s it took for the copy pill to appear after a paste missed.
# Served, each answer is a few tens of ms. See deliver.rs for the caller.
#
# The read itself is unchanged in both modes -- same function, same 180ms
# Chromium retry, same IsPassword bail-out.
#
# Comments here are read as ANSI by Windows PowerShell (this file has no BOM),
# so keep every string literal ASCII. Chinese in comments is fine; in a literal
# it would arrive mangled.
param([switch]$Serve)

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

# 读取当前焦点控件。除了 text，还要把"凭什么判断它可写"的证据一并带出去：
# 光返回一个 editable 布尔值，调用方分不清"确实只读"和"UIA 这次什么都没拿到"，
# 只能把后者也当成不可写，于是浏览器/Electron 里第一次听写就蹦药丸。
function Read-Focused {
  $el = [System.Windows.Automation.AutomationElement]::FocusedElement
  if (-not $el -or $el.Current.IsPassword) { return $null }

  $text = ''
  $caret = ''
  $editable = $false
  $hasValue = $false
  $valueReadOnly = $null
  $hasText = $false

  $value = $null
  if ($el.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$value)) {
    $hasValue = $true
    $valueReadOnly = [bool]$value.Current.IsReadOnly
    if (-not $valueReadOnly) { $text = $value.Current.Value; $editable = $true }
  }
  $pattern = $null
  if ($el.TryGetCurrentPattern([System.Windows.Automation.TextPattern]::Pattern, [ref]$pattern)) {
    $hasText = $true
    if (-not $editable) {
      $text = $pattern.DocumentRange.GetText(10000)
      $editable = $el.Current.IsKeyboardFocusable
    }
    # 插入点前面那一小段。上面那份 text 只取得到 DocumentRange 的前 10000 字，
    # 于是"粘在一篇长文末尾"和"压根没粘上"读回来一模一样——调用方只能一律当成
    # 粘上了，字就这么无声无息地没了（既没进输入框，也没上药丸）。
    # 插入点这一段不受那个截断影响：粘贴成功的话，刚落下的字必然紧挨在它前面。
    # 光标压根不在可编辑处（比如焦点掉回浏览器的页面正文）时，这里要么取不到、
    # 要么前后两次一模一样，同样是一条能定案的证据。
    try {
      $sel = $pattern.GetSelection()
      if ($sel -and $sel.Length -gt 0) {
        $r = $sel[0].Clone()
        # 先塌到选区起点，再往前拉 400 字。落下的字比这还长也不要紧：调用方比的是
        # 结果的末尾 12 字，而那 12 字就贴着插入点。
        $r.MoveEndpointByRange([System.Windows.Automation.Text.TextPatternRangeEndpoint]::End,
                               $sel[0], [System.Windows.Automation.Text.TextPatternRangeEndpoint]::Start)
        [void]$r.MoveEndpointByUnit([System.Windows.Automation.Text.TextPatternRangeEndpoint]::Start,
                                    [System.Windows.Automation.Text.TextUnit]::Character, -400)
        $caret = $r.GetText(400)
      }
    } catch {
      # SupportedTextSelection=None 的控件直接抛异常。拿不到就当没这条证据，
      # 绝不能让它影响上面那些字段——那些是主线。
      $caret = ''
    }
  }
  if ($null -eq $text) { $text = '' }
  if ($null -eq $caret) { $caret = '' }

  $pid2 = $el.Current.ProcessId
  $pname = ''
  try { $pname = (Get-Process -Id $pid2 -ErrorAction Stop).ProcessName } catch {}

  # "ControlType.Edit" → "Edit"
  $ctype = ''
  try { $ctype = ($el.Current.ControlType.ProgrammaticName -split '\.')[-1] } catch {}

  [pscustomobject]@{
    text              = [string]$text
    caret             = [string]$caret
    processId         = $pid2
    processName       = [string]$pname
    editable          = [bool]$editable
    automationId      = [string]$el.Current.AutomationId
    name              = [string]$el.Current.Name
    controlType       = [string]$ctype
    className         = [string]$el.Current.ClassName
    keyboardFocusable = [bool]$el.Current.IsKeyboardFocusable
    hasValue          = [bool]$hasValue
    valueReadOnly     = $valueReadOnly
    hasText           = [bool]$hasText
  }
}

function Get-FocusedJson {
  try {
    $r = Read-Focused
    # 两个 pattern 都没拿到 = 多半是 Chromium 还没把无障碍树建起来。它要等第一次
    # UIA 请求才开始建，而建的过程是异步的——上面那次查询正好把它踢起来了，
    # 等一下再读一次通常就能拿到真正的输入框。只在失败路径上付这 180ms。
    if ($r -and -not $r.hasValue -and -not $r.hasText) {
      Start-Sleep -Milliseconds 180
      $again = Read-Focused
      if ($again -and ($again.hasValue -or $again.hasText)) { $r = $again }
    }
    if ($null -eq $r) { return '{}' }
    return ($r | ConvertTo-Json -Compress)
  } catch {
    return '{}'
  }
}

# The caller parses this as UTF-8. Without this line a redirected stdout goes
# out in the console's OEM codepage (936 here), and any Chinese already sitting
# in the focused box comes back as bytes that are not valid UTF-8 -- the whole
# answer is then dropped and read as "UIA got nothing", which is exactly the
# state that makes the pill appear instead of pasting.
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false

if ($Serve) {
  # One request per line in, one JSON object per line out. Flush every time:
  # the caller is blocked on this line, and a buffered answer looks like a hang.
  # EOF (the app exited and closed the pipe) ends the loop, so this process
  # never outlives its caller.
  while ($null -ne ($line = [Console]::In.ReadLine())) {
    [Console]::Out.WriteLine((Get-FocusedJson))
    [Console]::Out.Flush()
  }
} else {
  [Console]::Out.WriteLine((Get-FocusedJson))
}
