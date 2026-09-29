<#
.SYNOPSIS
  用 RustDesk 自己的安装包（安装向导）就地升级，并在装完后逐文件哈希校验、清理残留。

.DESCRIPTION
  程序内升级与手动升级用同一份脚本。

  程序内（客户端点首页卡片）：客户端把安装包地址、哈希清单地址、修复包地址和**当前程序所在目录**
  传进来（-InstallPackageUrl -ManifestUrl -RepairZipUrl -InstallDir -Pause），以管理员权限运行：
    0/6 下载安装包与哈希清单
    1/6 备份当前安装目录（只保留最近 3 份）
    2/6 运行安装包 —— 会弹出安装向导；它是 libs/portable 打的自解压包，名字以 install.exe 结尾，
        所以解压后会带 --install 运行，安装目标目录取自注册表的 InstallLocation（因此装回现有目录，
        不会变成 Program Files 里的第二份）
    3/6 等安装落盘（轮询清单里 rustdesk.exe 的哈希，最多 8 分钟）
    4/6 逐文件哈希比对（清单里每一行都算一次）
    5/6 有不一致才修复：下载 unsigned 包，只覆盖不一致/缺失的文件，然后重新校验
    6/6 清理残留（打包器的解压缓存 %LOCALAPPDATA%\rustdesk 等）、起服务、同步注册表 BuildDate、
        以普通用户权限拉起界面

  手动（离线）：powershell -ExecutionPolicy Bypass -File upgrade-one-click.ps1 -SourceDir "D:\新版本\rustdesk"
  手动（安装向导）：powershell -ExecutionPolicy Bypass -File upgrade-one-click.ps1 -InstallPackageUrl "<installer>" -ManifestUrl "<sha256>" -InstallDir "E:\RustDesk"

  为什么不用"直接覆盖文件"当主路径：安装向导走的是 RustDesk 自己的安装流程（服务、快捷方式、注册表都它管），
  界面也更友好；但安装过程本身不做校验、还会在 %LOCALAPPDATA%\rustdesk 留一份解压缓存，
  所以这里补上逐文件哈希校验、按需修复和清理。
#>
param(
  # 空 = 从注册表推断（手动模式）；程序内升级总是显式传入。
  [string]$InstallDir = "",
  # 已解压好的新版本目录（离线/手动模式）。
  [string]$SourceDir = "",
  # 直接给 unsigned 包地址（老路径：下载后解压再覆盖，不做安装向导）。
  [string]$DownloadUrl = "",
  # 安装包地址（*install.exe）：走安装向导这条新路径。
  [string]$InstallPackageUrl = "",
  # 98 个文件的哈希清单地址（每行 "<sha256>  <相对路径>"）。
  [string]$ManifestUrl = "",
  # unsigned 包地址：只在哈希校验发现不一致时用来修复。
  [string]$RepairZipUrl = "",
  [string]$ServiceName = "RustDesk",
  [switch]$Pause
)

$ErrorActionPreference = 'Stop'

function Say($text, $color = "Cyan") { Write-Host $text -ForegroundColor $color }

function Get-Sha256($path) {
  return (Get-FileHash -Path $path -Algorithm SHA256).Hash.ToLower()
}

function Invoke-Download($url, $out) {
  $ProgressPreference = 'SilentlyContinue'
  Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing
}

function Read-Manifest($path) {
  $entries = @()
  foreach ($line in Get-Content -Path $path) {
    $t = $line.Trim()
    if (-not $t) { continue }
    $parts = $t -split '\s+', 2
    if ($parts.Count -lt 2) { continue }
    $entries += [pscustomobject]@{ Hash = $parts[0].ToLower(); Rel = $parts[1].Trim() }
  }
  return ,@($entries)
}

function Resolve-InstallDir {
  param([string]$Given)
  if ($Given) { return $Given.TrimEnd('\') }
  foreach ($key in @("HKLM:\SOFTWARE\RustDesk",
                     "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\RustDesk")) {
    try {
      $loc = (Get-ItemProperty -Path $key -Name InstallLocation -ErrorAction Stop).InstallLocation
      if ($loc -and (Test-Path (Join-Path $loc "rustdesk.exe"))) { return $loc.TrimEnd('\') }
    } catch { }
  }
  throw "找不到安装目录：请用 -InstallDir 指定 RustDesk 所在目录（例如 -InstallDir `"D:\RustDesk`"）。"
}

function Stop-AllRustDesk {
  $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
  if ($svc -and $svc.Status -ne 'Stopped') {
    Say "  停止服务 $ServiceName ..."
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
  }
  for ($round = 1; $round -le 3; $round++) {
    $procs = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
             Where-Object { $_.Name -like "*rustdesk*" }
    if (-not $procs) { break }
    foreach ($p in $procs) {
      Say ("  结束进程 {0} ({1})" -f $p.ProcessId, $p.Name) "DarkGray"
      try { Invoke-CimMethod -InputObject $p -MethodName Terminate -ErrorAction Stop | Out-Null } catch { }
    }
    Start-Sleep -Seconds 3
  }
  $left = Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object { $_.Name -like "*rustdesk*" }
  if ($left) { Say ("  警告：仍有 {0} 个进程没结束，覆盖可能失败" -f ($left | Measure-Object).Count) "Yellow" }
}

function Wait-ForInstall($installDir, $entries, $startedAt, $timeoutSeconds = 600) {
  # 打包器解压完就把 --install 交给子进程（子进程自己就从解压目录 %LOCALAPPDATA%\rustdesk 里跑），
  # 父进程随即退出。所以"装完"的判据是：解压目录里已经没有进程在跑（连续两次），
  # 并且安装目录里的 librustdesk.dll 确实是这次新写进去的（时间戳新于本次开始、哈希与清单一致）。
  # ——这一步以前太乐观：同版本重装时目标哈希一开始就相同，于是立刻往下走，
  #   5/6 的清理把安装程序自己的解压目录删掉，安装就中断了（实测踩到过）。
  $cache = ''
  if ($env:LOCALAPPDATA) { $cache = Join-Path $env:LOCALAPPDATA 'rustdesk' }
  $target = $entries | Where-Object { $_.Rel -eq 'librustdesk.dll' } | Select-Object -First 1
  $deadline = (Get-Date).AddSeconds($timeoutSeconds)
  $idle = 0
  while ((Get-Date) -lt $deadline) {
    $busy = 0
    if ($cache) {
      $busy = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
        $_.Name -like '*rustdesk*' -and $_.ExecutablePath -and
        $_.ExecutablePath.StartsWith($cache, 'OrdinalIgnoreCase')
      }).Count
    }
    if ($busy -eq 0) { $idle++ } else { $idle = 0 }
    $landed = $true
    if ($target) {
      $dst = Join-Path $installDir $target.Rel
      $landed = (Test-Path $dst) -and ((Get-Item $dst).LastWriteTime -gt $startedAt) -and ((Get-Sha256 $dst) -eq $target.Hash)
    }
    if ($idle -ge 2 -and $landed) { return $true }
    Start-Sleep -Seconds 3
  }
  return $false
}

function Compare-Install($installDir, $entries) {
  $bad = @()
  foreach ($e in $entries) {
    $dst = Join-Path $installDir ($e.Rel -replace '/', '\')
    if (-not (Test-Path $dst)) { $bad += $e; continue }
    if ((Get-Sha256 $dst) -ne $e.Hash) { $bad += $e }
  }
  # 空数组直接 return 会被 PowerShell 展开成 $null，调用处 .Count 就成了空 —— 包一层 @()。
  return ,@($bad)
}

function Get-SourceFromUrl {
  param([string]$Url)
  $work = Join-Path $env:TEMP ("rustdesk-upgrade-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
  New-Item -ItemType Directory -Force -Path $work | Out-Null
  $zip = Join-Path $work "package.zip"
  Say ("  下载   : {0}" -f $Url) "DarkGray"
  Invoke-Download $Url $zip
  Say ("  已下载 : {0:N1} MB" -f ((Get-Item $zip).Length / 1MB)) "DarkGray"
  $out = Join-Path $work "files"
  Expand-Archive -Path $zip -DestinationPath $out -Force
  Remove-Item $zip -Force -ErrorAction SilentlyContinue
  return $out
}

Say "=== RustDesk 一键升级 ==="
$scriptStart = Get-Date
# 升级前就已经在跑的界面进程：判断"界面是不是这次新起来的"时要排除它们，
# 否则同版本重装（进程一直没死）会误报"界面已启动"。
$preUiPids = @(Get-Process rustdesk -ErrorAction SilentlyContinue |
               Where-Object { $_.SessionId -ne 0 } | ForEach-Object { $_.Id })
$work = $null
$entries = @()
$useWizard = [bool]$InstallPackageUrl

if ($useWizard) {
  $work = Join-Path $env:TEMP ("rustdesk-upgrade-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
  New-Item -ItemType Directory -Force -Path $work | Out-Null
  Say "0/6 下载安装包与哈希清单"
  # 必须保留安装包原本的文件名：名字以 install.exe 结尾才是"走安装流程"的开关
  # （libs/portable 的 click_setup 判断的就是文件名）。存成 installer.exe 之类就只是解压后当便携版跑，
  # 既不会弹安装界面也不会 --install —— 实测踩到过。
  $installerName = Split-Path ([uri]$InstallPackageUrl).AbsolutePath -Leaf
  if ($installerName -notlike '*install.exe') {
    Say ("  警告：安装包文件名 '{0}' 不以 install.exe 结尾，安装向导不会出现（这是打包器的开关）" -f $installerName) "Yellow"
  }
  $installer = Join-Path $work $installerName
  Invoke-Download $InstallPackageUrl $installer
  Say ("  安装包 : {0:N1} MB" -f ((Get-Item $installer).Length / 1MB)) "DarkGray"
  $manifest = Join-Path $work "files.sha256"
  Invoke-Download $ManifestUrl $manifest
  $entries = Read-Manifest $manifest
  Say ("  清单   : {0} 个文件" -f $entries.Count) "DarkGray"
} elseif ($DownloadUrl) {
  Say "0/6 下载新版本包"
  $SourceDir = Get-SourceFromUrl -Url $DownloadUrl
} elseif (-not $SourceDir) {
  # Default for the manual flow: the extraction of the last downloaded build.
  $SourceDir = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "rustdesk-build\extracted\unsigned"
}
$InstallDir = Resolve-InstallDir -Given $InstallDir
Say ("  安装目录: {0}" -f $InstallDir)

if (-not $useWizard) {
  if (-not (Test-Path (Join-Path $SourceDir "rustdesk.exe"))) {
    throw "源目录里没有 rustdesk.exe：$SourceDir"
  }
  # 没有清单时，用源目录自己生成一份，后面照样逐文件校验。
  $entries = Get-ChildItem $SourceDir -Recurse -File | ForEach-Object {
    [pscustomobject]@{ Hash = (Get-Sha256 $_.FullName); Rel = $_.FullName.Substring($SourceDir.Length + 1).Replace('\', '/') }
  }
} elseif (-not (Test-Path (Join-Path $InstallDir "rustdesk.exe"))) {
  Say "  提示：安装目录里还没有 rustdesk.exe，按全新安装处理" "Yellow"
}

$oldVer = ""
if (Test-Path (Join-Path $InstallDir "rustdesk.exe")) {
  $oldVer = (Get-Item (Join-Path $InstallDir "rustdesk.exe")).VersionInfo.FileVersion
}

Say "1/6 备份安装目录"
if (Test-Path (Join-Path $InstallDir "rustdesk.exe")) {
  $backup = "{0}.bak-{1}-{2}" -f $InstallDir, (Get-Date -Format 'yyyyMMdd-HHmmss'), ($oldVer -replace '[^\w\.\-]', '')
  Copy-Item -Path $InstallDir -Destination $backup -Recurse -Force
  Say ("  备份到 {0}" -f $backup) "DarkGray"
} else {
  $backup = "(无，之前没有安装)"
  Say "  跳过（之前没有安装）" "DarkGray"
}

if ($useWizard) {
  Say "2/6 运行安装包（会弹出安装向导，请按提示完成）"
  Say ("  {0}" -f $installer) "DarkGray"
  # 提权环境里运行，不再二次 UAC。父进程（打包器）解压完就退出，安装由子进程继续。
  $p = Start-Process -FilePath $installer -PassThru
  $p | Wait-Process -Timeout 300 -ErrorAction SilentlyContinue
  Say "  等待安装完成 …"
  $landed = Wait-ForInstall -installDir $InstallDir -entries $entries -startedAt $scriptStart
  if (-not $landed) { Say "  等到超时，仍继续做逐文件校验" "Yellow" }
} else {
  Say "2/6 覆盖文件（离线/手动模式）"
  Stop-AllRustDesk
  Copy-Item -Path (Join-Path $SourceDir '*') -Destination $InstallDir -Recurse -Force
}

Say "3/6 逐文件哈希校验"
$bad = Compare-Install -installDir $InstallDir -entries $entries
if ($bad.Count -eq 0) {
  Say ("  {0} 个文件全部一致" -f $entries.Count) "Green"
} else {
  Say ("  有 {0}/{1} 个文件不一致" -f $bad.Count, $entries.Count) "Yellow"
}

if ($bad.Count -gt 0) {
  Say "4/6 修复不一致的文件"
  $repairSource = $SourceDir
  if (-not $repairSource -or -not (Test-Path (Join-Path $repairSource 'rustdesk.exe'))) {
    if (-not $RepairZipUrl) { throw ("有 {0} 个文件不一致，但没给修复包地址（-RepairZipUrl）" -f $bad.Count) }
    $repairSource = Get-SourceFromUrl -Url $RepairZipUrl
  }
  foreach ($e in $bad) {
    $src = Join-Path $repairSource ($e.Rel -replace '/', '\')
    $dst = Join-Path $InstallDir ($e.Rel -replace '/', '\')
    if (-not (Test-Path $src)) { Say ("  跳过（修复包里也没有）：{0}" -f $e.Rel) "Yellow"; continue }
    $dir = Split-Path $dst -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    Copy-Item -Path $src -Destination $dst -Force
  }
  $bad = Compare-Install -installDir $InstallDir -entries $entries
  if ($bad.Count -eq 0) { Say ("  修复后 {0} 个文件全部一致" -f $entries.Count) "Green" }
  else { Say ("  仍有 {0} 个文件不一致（可能被占用）" -f $bad.Count) "Red" }
} else {
  Say "4/6 无需修复"
}

Say "5/6 清理残留"
# 打包器的解压缓存：官方安装包会在 %LOCALAPPDATA%\<app> 留一份解压副本，这里清掉，避免"第二份安装"的错觉。
$cacheDirs = @()
if ($env:LOCALAPPDATA) { $cacheDirs += (Join-Path $env:LOCALAPPDATA 'rustdesk') }
foreach ($d in $cacheDirs) {
  if (Test-Path $d) {
    Remove-Item -Path $d -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $d) { Say ("  没能删除 {0}（可能正被占用）" -f $d) "Yellow" }
    else { Say ("  已删除打包器缓存 {0}" -f $d) "DarkGray" }
  }
}
# 安装目录之外的 RuntimeBroker_rustdesk.exe（打包器会顺手拷一份到解压目录，已经随缓存删掉；
# 这里再扫一遍常见位置，避免留下散落文件）
foreach ($root in @($env:LOCALAPPDATA, $env:TEMP)) {
  if (-not $root -or -not (Test-Path $root)) { continue }
  Get-ChildItem $root -Filter 'RuntimeBroker_rustdesk.exe' -Recurse -File -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue; Say ("  已删除散落文件 {0}" -f $_.FullName) "DarkGray" }
}
# 备份只留最近 3 份
$keepBackups = 3
$backupDirs = Get-ChildItem (Split-Path $InstallDir -Parent) -Directory `
    -Filter ("{0}.bak-*" -f (Split-Path $InstallDir -Leaf)) -ErrorAction SilentlyContinue |
  Sort-Object Name -Descending
if ($backupDirs.Count -gt $keepBackups) {
  $backupDirs | Select-Object -Skip $keepBackups | ForEach-Object {
    Say ("  清理旧备份 {0}" -f $_.Name) "DarkGray"
    Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
  }
}
# 确认没有多出第二份安装（只报告，不擅自删除别处的安装）
Say "  安装副本检查："
foreach ($root in @('C:\Program Files', 'C:\Program Files (x86)', $env:LOCALAPPDATA, 'E:\')) {
  if (-not $root -or -not (Test-Path $root)) { continue }
  Get-ChildItem $root -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like '*ustDesk*' -and $_.Name -notlike '*.bak-*' } |
    Where-Object { Test-Path (Join-Path $_.FullName 'rustdesk.exe') } |
    Where-Object { $_.FullName.TrimEnd('\') -ne $InstallDir.TrimEnd('\') } |
    ForEach-Object { Say ("    另外还有一份：{0}" -f $_.FullName) "Yellow" }
}

Say "6/6 收尾：服务、注册表、界面"
try {
  $ver = (Get-Item (Join-Path $InstallDir "rustdesk.exe")).VersionInfo.FileVersion
  $buildDate = ""
  $dllPath = Join-Path $InstallDir "librustdesk.dll"
  if (Test-Path $dllPath) {
    # BUILD_DATE 由 hbb_common::gen_version() 以 "yyyy-MM-dd HH:mm" 写进库里；首页那张
    # "安装版本偏低"的卡片正是拿它和注册表里的 BuildDate 比较，抄丢一项就会一直提示。
    # 用 GetEncoding(28591) 而不是 Encoding::Latin1：后者只有 .NET Core/5+ 才有，
    # 而程序内升级跑的是 Windows PowerShell 5.1，那里 Latin1 是 null。
    $text = [System.Text.Encoding]::GetEncoding(28591).GetString([System.IO.File]::ReadAllBytes($dllPath))
    $dates = [regex]::Matches($text, '20\d\d-\d\d-\d\d \d\d:\d\d') | ForEach-Object { $_.Value } | Sort-Object -Unique
    if ($dates) { $buildDate = $dates | Sort-Object -Descending | Select-Object -First 1 }
  }
  $uninstallString = '"{0}" --uninstall' -f (Join-Path $InstallDir "rustdesk.exe")
  foreach ($key in @("HKLM:\SOFTWARE\RustDesk", "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\RustDesk")) {
    New-Item -Path $key -Force | Out-Null
    New-ItemProperty -Path $key -Name DisplayName -Value "RustDesk" -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name DisplayVersion -Value $ver -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name Version -Value $ver -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name InstallLocation -Value $InstallDir -PropertyType String -Force | Out-Null
    New-ItemProperty -Path $key -Name UninstallString -Value $uninstallString -PropertyType String -Force | Out-Null
    if ($buildDate) { New-ItemProperty -Path $key -Name BuildDate -Value $buildDate -PropertyType String -Force | Out-Null }
  }
  Say ("  版本={0}  BuildDate={1}" -f $ver, $buildDate) "DarkGray"
} catch {
  Say ("  注册表同步失败（不影响升级本身）：{0}" -f $_.Exception.Message) "Yellow"
}

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc) {
  for ($i = 1; $i -le 3; $i++) {
    try { Start-Service -Name $ServiceName -ErrorAction Stop; break }
    catch { Say ("  启动服务失败（第 {0} 次）：{1}" -f $i, $_.Exception.Message) "Yellow"; Start-Sleep -Seconds 3 }
  }
  Start-Sleep -Seconds 3
}

# 界面必须用**普通用户权限**启动：本脚本是提权跑的，直接 Start-Process 会以管理员身份启动，
# 而 RustDesk 的界面不接受提权运行——它会立刻自己退出，表现就是"升级完没有自己拉起"。
# explorer.exe 转发在提权环境里也不一定生效（实测过），所以改用计划任务：指定交互用户 +
# RunLevel Limited，拿到的就是普通用户令牌，和双击完全一样。失败再逐级降级并给出手动提示。
$exe = Join-Path $InstallDir "rustdesk.exe"
function Get-UiProcessCount {
  @(Get-Process rustdesk -ErrorAction SilentlyContinue |
    Where-Object { $_.SessionId -ne 0 -and $preUiPids -notcontains $_.Id }).Count
}
$started = $false
$taskName = "RustDeskUpgradeRelaunch"
$interactiveUser = (Get-CimInstance Win32_ComputerSystem -ErrorAction SilentlyContinue).UserName
if (-not $interactiveUser) { $interactiveUser = "$env:USERDOMAIN\$env:USERNAME" }
try {
  $action = New-ScheduledTaskAction -Execute $exe -WorkingDirectory $InstallDir
  $principal = New-ScheduledTaskPrincipal -UserId $interactiveUser -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Force | Out-Null
  Start-ScheduledTask -TaskName $taskName
  Start-Sleep -Seconds 12
  $started = (Get-UiProcessCount) -gt 0
  if ($started) { Say ("  已用普通用户权限启动界面（{0}）" -f $interactiveUser) "DarkGray" }
} catch {
  Say ("  计划任务启动界面失败：{0}" -f $_.Exception.Message) "Yellow"
} finally {
  Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
}
if (-not $started) {
  try {
    Start-Process explorer.exe -ArgumentList $exe
    Start-Sleep -Seconds 8
    $started = (Get-UiProcessCount) -gt 0
  } catch { }
}
if (-not $started) {
  Say "  换直接启动再试一次（可能会被提权拦下）" "Yellow"
  try { Start-Process $exe -WorkingDirectory $InstallDir; Start-Sleep -Seconds 8 } catch { }
  $started = (Get-UiProcessCount) -gt 0
}
if ($started) { Say "  界面已启动" "Green" }
else { Say ("  界面没有自动起来，请手动双击：{0}" -f $exe) "Yellow" }

Say ""
if ($bad.Count -eq 0) { Say ("完成：{0} 个文件全部一致，备份在 {1}" -f $entries.Count, $backup) "Green" }
else { Say ("部分完成：还有 {0} 个文件不一致；备份在 {1}" -f $bad.Count, $backup) "Yellow" }
if ($work -and (Test-Path $work)) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
if ($svc) { Get-Service -Name $ServiceName | Select-Object Name, Status, StartType | Format-Table -AutoSize }

if ($Pause) {
  # Launched from the app: let the user read the result before the window goes away.
  Say "这个窗口 15 秒后自动关闭" "DarkGray"
  Start-Sleep -Seconds 15
}
