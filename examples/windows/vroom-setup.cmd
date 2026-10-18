@echo off
rem vroom guest tools for Windows ARM64: virtio drivers, agent, shared
rem folders. Looks for the virtio-win media on any drive letter. Run as
rem admin (first logon runs it elevated). Safe to run again to upgrade.
rem No labels or goto: cmd can miss labels in files with Unix line ends.
rem Started by a double click: ask for administrator rights and start
rem again (first logon runs it elevated already).
net session >nul 2>&1
if errorlevel 1 (
  powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0'"
  exit /b
)
set VIOFS=
for %%d in (D E F G H I J K) do (
  if exist %%d:\viogpudo\w11\ARM64 (
    for %%x in (viogpudo NetKVM vioserial vioinput viorng viostor vioscsi viofs Balloon) do (
      if exist %%d:\%%x\w11\ARM64 pnputil /add-driver %%d:\%%x\w11\ARM64\*.inf /install
    )
    if exist %%d:\viofs\w11\ARM64\virtiofs.exe set VIOFS=%%d:\viofs\w11\ARM64\virtiofs.exe
  )
)
rem Our RTC runs in UTC (like every hypervisor's); Windows assumes local.
reg add HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation /v RealTimeIsUniversal /t REG_DWORD /d 1 /f
rem Agent, split in two:
rem  - service "VroomAgent" (LocalSystem): owns the VirtIO port (only
rem    admins / SYSTEM may open it), clock sync, clean shutdown;
rem  - "--session" helper, unprivileged, started in each user session:
rem    clipboard and resolution, through a pipe only the service serves.
if exist "%~dp0vroom-agent.exe" (
  sc stop VroomAgent >nul 2>&1
  taskkill /f /im vroom-agent.exe >nul 2>&1
  rem The project used to be called bulwark: remove that agent.
  sc stop BulwarkAgent >nul 2>&1
  sc delete BulwarkAgent >nul 2>&1
  schtasks /end /tn "Bulwark Agent" >nul 2>&1
  schtasks /delete /f /tn "Bulwark Agent" >nul 2>&1
  taskkill /f /im bulwark-agent.exe >nul 2>&1
  reg delete HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkSession /f >nul 2>&1
  reg delete HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkAgent /f >nul 2>&1
  rem taskkill doesn't wait for the process to go: the copy would fail.
  timeout /t 3 /nobreak >nul
  rmdir /s /q "%ProgramFiles%\Bulwark" 2>nul
  mkdir "%ProgramFiles%\Vroom" 2>nul
  copy /y "%~dp0vroom-agent.exe" "%ProgramFiles%\Vroom\" || (echo vroom: could not copy the agent & exit /b 1)
  rem OpenGL and Vulkan for apps (3D display only): the agent registers
  rem them with the 3D display device when it starts.
  if exist "%~dp0vroom_gl.dll" copy /y "%~dp0vroom_gl*.dll" "%ProgramFiles%\Vroom\" >nul
  if exist "%~dp0vroom_vk_arm64.json" copy /y "%~dp0vroom_vk_*.*" "%ProgramFiles%\Vroom\" >nul
  rem Direct3D: a wrapper around the 3D driver's own, so apps survive a
  rem suspend. The agent registers it too.
  if exist "%~dp0vroom_d3d.dll" copy /y "%~dp0vroom_d3d*.dll" "%ProgramFiles%\Vroom\" >nul
  sc query VroomAgent >nul 2>&1 || sc create VroomAgent binPath= "\"%ProgramFiles%\Vroom\vroom-agent.exe\" --service" start= auto obj= LocalSystem DisplayName= "Vroom guest agent"
  sc description VroomAgent "Host channel for the vroom VMM: clock sync, clean shutdown, relays clipboard and resolution to the session helper."
  sc failure VroomAgent reset= 86400 actions= restart/5000/restart/5000/restart/30000
)
rem Shared folders: virtio-win's virtiofs.exe on WinFsp. Each [[share]]
rem becomes a service "VroomShare_<tag>" that mounts it on a drive letter
rem (Z: first). The agent service does the same when it starts, so
rem shares added later show up after a reboot.
set SHARES=
if defined VIOFS if exist "%ProgramFiles%\Vroom\vroom-agent.exe" set SHARES=1
rem Running share services hold virtiofs.exe open.
if defined SHARES for /f "tokens=2" %%s in ('sc query state^= all ^| findstr /b /c:"SERVICE_NAME: VroomShare_"') do sc stop %%s >nul 2>&1
if defined SHARES timeout /t 2 /nobreak >nul
if defined SHARES copy /y "%VIOFS%" "%ProgramFiles%\Vroom\" >nul || set SHARES=
rem WinFsp (winfsp.dev, GPLv3 with a FLOSS exception): its MSI comes next
rem to this script (scripts/fetch.sh pins it); the SHA-256 must match.
set WINFSP_MSI=winfsp-2.1.25156.msi
set WINFSP_SHA256=073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a
set WINFSP_TMP=%TEMP%\%WINFSP_MSI%
set NEED=
if defined SHARES if not exist "%ProgramFiles(x86)%\WinFsp\bin\winfsp-a64.dll" set NEED=1
if defined NEED del "%WINFSP_TMP%" 2>nul
if defined NEED if exist "%~dp0%WINFSP_MSI%" copy /y "%~dp0%WINFSP_MSI%" "%WINFSP_TMP%" >nul
if defined NEED (
  certutil -hashfile "%WINFSP_TMP%" SHA256 | findstr /i /x "%WINFSP_SHA256%" >nul || (echo vroom: no WinFsp ^(missing or wrong hash^), no shared folders & set SHARES=)
)
if defined NEED if defined SHARES msiexec /i "%WINFSP_TMP%" /qn /norestart
if defined NEED del "%WINFSP_TMP%" 2>nul
if defined SHARES "%ProgramFiles%\Vroom\vroom-agent.exe" --mount-shares
if exist "%~dp0vroom-agent.exe" (
  sc start VroomAgent
  reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v VroomSession /t REG_SZ /d "\"%ProgramFiles%\Vroom\vroom-agent.exe\" --session" /f
  rem This session: start the helper without our admin token.
  runas /trustlevel:0x20000 "\"%ProgramFiles%\Vroom\vroom-agent.exe\" --session"
)
echo done > C:\vroom-setup.done
