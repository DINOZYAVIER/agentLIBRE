<# : batch section, which PowerShell reads as a comment
@echo off
rem agl on Windows, on NixOS-WSL, from this one file (AGL-057). Double-click it.
rem Run again after a failure, it goes on from where it stopped.
rem An argument names another flake, as github:DINOZYAVIER/agentLIBRE/develop.
set "AGL_INSTALL_SELF=%~f0"
set "AGL_INSTALL_FLAKE=%~1"
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "Invoke-Expression ([IO.File]::ReadAllText($env:AGL_INSTALL_SELF))"
set "AGL_INSTALL_STATUS=%errorlevel%"
echo.
pause
exit /b %AGL_INSTALL_STATUS%
#>

$ErrorActionPreference = 'Continue'
$env:WSL_UTF8 = '1'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$distro = 'nixos-wsl-agl'
$nixosWslUrl = 'https://github.com/nix-community/NixOS-WSL/releases/download/2605.7.2/nixos.wsl'
$nixosWslSha256 = 'e7180ad555fdcb8e1e057e2ef056de467603a5e502ff8531053738371be3f6b9'
$nixosWslSize = 577813513
$minimumBuild = 22621
$flake = if ($env:AGL_INSTALL_FLAKE) { $env:AGL_INSTALL_FLAKE } else { 'github:DINOZYAVIER/agentLIBRE' }

$dir = Join-Path $env:LOCALAPPDATA 'agentLIBRE\install'
$self = Join-Path $dir 'install-agl.cmd'
$log = Join-Path $dir 'install.log'
$restartMarker = Join-Path $dir 'restart-requested'
$runOnceKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\RunOnce'
$runOnceName = 'agentLIBRE install'

New-Item -ItemType Directory -Force -Path $dir | Out-Null

function Write-Log([string]$Text) {
  Write-Host $Text
  Add-Content -Path $log -Value $Text -Encoding UTF8
}

function Stop-Install([string]$Text) {
  Write-Log ''
  Write-Log "FAILED: $Text"
  Write-Log "The log: $log"
  Write-Log 'Fix this and run the file again: it goes on from where it stopped.'
  exit 1
}

# Output on the screen and in the log. Returns the exit code of the program.
function Invoke-Logged([string]$Program, [string[]]$Arguments) {
  & $Program @Arguments 2>&1 | ForEach-Object {
    $line = "$_"
    # wsl.exe leaves the console with a line feed that does not return the cursor.
    Write-Host "`r$line"
    Add-Content -Path $log -Value $line -Encoding UTF8
  }
  return $LASTEXITCODE
}

# A script for NixOS, with the LF that bash needs, in the folder WSL starts in.
function Write-LinuxScript([string]$Name, [string]$Text) {
  [IO.File]::WriteAllText((Join-Path $dir $Name), $Text.Replace("`r`n", "`n"), (New-Object Text.ASCIIEncoding))
}

# The wsl.exe of System32 is a stub with no WSL: asked anything, it waits 60 s for a key
# that would install it. The real one is looked for instead.
function Test-WslProgram {
  $program = Join-Path $env:ProgramFiles 'WSL\wsl.exe'
  if (-not (Test-Path $program)) { return $false }
  & $program --version *> $null
  return ($LASTEXITCODE -eq 0)
}

function Test-VirtualMachinePlatform {
  $feature = Get-CimInstance Win32_OptionalFeature -Filter "Name='VirtualMachinePlatform'"
  return ($null -ne $feature -and $feature.InstallState -eq 1)
}

function Test-Distro {
  & wsl.exe -d $distro -- true *> $null
  return ($LASTEXITCODE -eq 0)
}

function Get-BootTime {
  return (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')
}

function Request-Restart {
  $line = "cmd.exe /c `"`"$self`""
  if ($env:AGL_INSTALL_FLAKE) { $line += " `"$($env:AGL_INSTALL_FLAKE)`"" }
  $line += '"'
  if (-not (Test-Path $runOnceKey)) { New-Item -Path $runOnceKey -Force | Out-Null }
  Set-ItemProperty -Path $runOnceKey -Name $runOnceName -Value $line
  Write-Log ''
  Write-Log 'Restart Windows now. After you log in, the install goes on by itself.'
  exit 0
}

# --- This file, kept where it survives the restart.
if ($env:AGL_INSTALL_SELF -ne $self) {
  Copy-Item -Force -Path $env:AGL_INSTALL_SELF -Destination $self
  Unblock-File -Path $self
}
Remove-ItemProperty -Path $runOnceKey -Name $runOnceName -ErrorAction SilentlyContinue

Write-Log ''
Write-Log "=== agl install, $(Get-Date -Format o), from $flake"

# --- The conditions.
$build = [Environment]::OSVersion.Version.Build
Write-Log "Windows build: $build"
if ($build -lt $minimumBuild) {
  Stop-Install 'agl needs Windows 11 22H2 or later, for the mirrored networking of WSL.'
}
if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
  Stop-Install "agl needs an x86-64 machine, and this one is $($env:PROCESSOR_ARCHITECTURE)."
}
$hypervisor = (Get-CimInstance Win32_ComputerSystem).HypervisorPresent
$firmware = (Get-CimInstance Win32_Processor | Select-Object -First 1).VirtualizationFirmwareEnabled
if (-not ($hypervisor -or $firmware)) {
  Stop-Install 'Virtualization is off. Turn on SVM or VT-x in the BIOS of this machine.'
}
$gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name })
Write-Log "Video: $($gpus -join ', ')"
$noGpuDriver = @($gpus | Where-Object { $_ -notmatch 'Microsoft Basic' }).Count -eq 0
if ($noGpuDriver) {
  Write-Log 'No driver of a GPU is installed. agl can run on the CPU only, until there is one.'
}

# --- The networking of WSL. A file that exists is not changed.
$wslConfig = Join-Path $env:USERPROFILE '.wslconfig'
if (-not (Test-Path $wslConfig)) {
  Set-Content -Path $wslConfig -Value '[wsl2]', 'networkingMode=mirrored' -Encoding ASCII
  Write-Log "Written: $wslConfig"
  Write-Log 'agl needs the mirrored networking of WSL. It applies to all your WSL distributions.'
}

# --- WSL.
$platformBefore = Test-VirtualMachinePlatform
if ((Test-WslProgram) -and $platformBefore) {
  Remove-Item -Path $restartMarker -ErrorAction SilentlyContinue
} else {
  if (Test-Path $restartMarker) {
    if ((Get-Content -Path $restartMarker -Raw).Trim() -eq (Get-BootTime)) {
      Write-Log 'WSL is waiting for the restart of Windows.'
      Request-Restart
    }
    Remove-Item -Path $restartMarker
    Stop-Install 'WSL is not working after the restart. To see why, run this in a terminal as administrator: wsl --install --no-distribution'
  }
  Write-Log 'Installing WSL. Windows asks for administrator rights.'
  try {
    $install = Start-Process -FilePath wsl.exe -ArgumentList '--install', '--no-distribution' -Verb RunAs -Wait -PassThru
  } catch {
    Stop-Install "The install of WSL did not start: $_"
  }
  Write-Log "wsl --install exit $($install.ExitCode)"
  if ($install.ExitCode -ne 0) {
    Stop-Install 'WSL did not install. To see why, run this in a terminal as administrator: wsl --install --no-distribution'
  }
  # A feature of Windows that was off takes effect only after the restart.
  if (-not ($platformBefore -and (Test-WslProgram))) {
    Set-Content -Path $restartMarker -Value (Get-BootTime)
    Request-Restart
  }
}

# --- NixOS.
if (Test-Distro) {
  Write-Log "$distro is installed."
} else {
  $image = Join-Path $dir 'nixos.wsl'
  $hashOk = (Test-Path $image) -and ((Get-FileHash -Path $image -Algorithm SHA256).Hash -eq $nixosWslSha256)
  # A whole file of a wrong hash cannot be resumed: the server refuses the range.
  if ((-not $hashOk) -and (Test-Path $image) -and ((Get-Item $image).Length -ge $nixosWslSize)) {
    Remove-Item -Path $image
  }
  if (-not $hashOk) {
    Write-Log "Downloading NixOS-WSL, 578 MB."
    # A login has just happened after a restart: the network may be coming up.
    & curl.exe -fL --retry 10 --retry-all-errors --retry-delay 6 -C - -o $image $nixosWslUrl
    if ($LASTEXITCODE -ne 0) {
      Stop-Install "The download of $nixosWslUrl failed, curl exit $LASTEXITCODE."
    }
    if ((Get-FileHash -Path $image -Algorithm SHA256).Hash -ne $nixosWslSha256) {
      Remove-Item -Path $image
      Stop-Install 'The sha256 of nixos.wsl differs from the expected one. The file is removed.'
    }
  }
  [void](Invoke-Logged 'wsl.exe' @('--update'))
  $status = Invoke-Logged 'wsl.exe' @('--install', '--from-file', $image, '--name', $distro, '--no-launch')
  if (-not (Test-Distro)) {
    Stop-Install "$distro did not install: wsl exit $status."
  }
  Remove-Item -Path $image
}

# --- agl: 0 it works, 1 it is not there, 2 WSL does not run mirrored.
Write-LinuxScript 'agl-check.sh' @'
#!/bin/bash
export PATH=/run/current-system/sw/bin:$PATH
for program in agl wslinfo wslpath git; do
  command -v "$program" >/dev/null || { echo "$program is not there"; exit 1; }
done
agl --version || exit 1
wslpath -w /nix/store || exit 1
mode=$(wslinfo --networking-mode)
echo "networking mode: $mode"
[ "$mode" = mirrored ] || exit 2
'@
Write-LinuxScript 'agl-system.sh' @'
#!/bin/bash
export PATH=/run/current-system/sw/bin:$PATH
nixos-rebuild switch --flake "$1#wsl" --refresh
echo "nixos-rebuild exit $?"
'@

function Invoke-Check {
  return (Invoke-Logged 'wsl.exe' @('-d', $distro, '--cd', $dir, '--', '/bin/bash', './agl-check.sh'))
}

& wsl.exe -d $distro --cd $dir -- /bin/bash ./agl-check.sh *> $null
if ($LASTEXITCODE -eq 1) {
  Write-Log 'Building agl and its system from source. This takes a while.'
  # What decides is the check below, not the exit code of the switch.
  [void](Invoke-Logged 'wsl.exe' @('-d', $distro, '-u', 'root', '--cd', $dir, '--', '/bin/bash', './agl-system.sh', $flake))
  [void](Invoke-Logged 'wsl.exe' @('--terminate', $distro))
}

$check = Invoke-Check
if ($check -eq 2) {
  Stop-Install "WSL does not run mirrored. Have networkingMode=mirrored under [wsl2] in $wslConfig, then run: wsl --shutdown"
}
if ($check -ne 0) {
  Stop-Install "agl is not working in $distro, check exit $check."
}

Remove-Item -Path $restartMarker -ErrorAction SilentlyContinue
Write-Log ''
Write-Log 'agl is ready. To start it, in a terminal:'
Write-Log ''
Write-Log "  wsl -d $distro"
if ($noGpuDriver) {
  Write-Log '  agl function use agentLIBRE-cpu-low'
}
Write-Log '  agl'
Write-Log ''
Write-Log 'At its first start, agl offers to download the model.'
exit 0
