@echo off
rem bulwark guest tools for Windows ARM64: virtio drivers + agent.
rem Looks for the virtio-win media on any drive letter. Run as admin
rem (first logon runs it elevated). Safe to run again to upgrade.
for %%d in (D E F G H I J K) do (
  if exist %%d:\viogpudo\w11\ARM64 (
    for %%x in (viogpudo NetKVM vioserial vioinput viorng viostor) do (
      pnputil /add-driver %%d:\%%x\w11\ARM64\*.inf /install
    )
  )
)
rem Our RTC runs in UTC (like every hypervisor's); Windows assumes local.
reg add HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation /v RealTimeIsUniversal /t REG_DWORD /d 1 /f
rem Agent, split in two:
rem  - service "BulwarkAgent" (LocalSystem): owns the VirtIO port (only
rem    admins / SYSTEM may open it), clock sync, clean shutdown;
rem  - "--session" helper, unprivileged, started in each user session:
rem    clipboard and resolution, through a pipe only the service serves.
if exist "%~dp0bulwark-agent.exe" (
  sc stop BulwarkAgent >nul 2>&1
  schtasks /end /tn "Bulwark Agent" >nul 2>&1
  taskkill /f /im bulwark-agent.exe >nul 2>&1
  rem taskkill doesn't wait for the process to go: the copy would fail.
  timeout /t 3 /nobreak >nul
  mkdir "%ProgramFiles%\Bulwark" 2>nul
  copy /y "%~dp0bulwark-agent.exe" "%ProgramFiles%\Bulwark\" || (echo bulwark: could not copy the agent & exit /b 1)
  sc query BulwarkAgent >nul 2>&1 || sc create BulwarkAgent binPath= "\"%ProgramFiles%\Bulwark\bulwark-agent.exe\" --service" start= auto obj= LocalSystem DisplayName= "Bulwark guest agent"
  sc description BulwarkAgent "Host channel for the bulwark VMM: clock sync, clean shutdown, relays clipboard and resolution to the session helper."
  sc failure BulwarkAgent reset= 86400 actions= restart/5000/restart/5000/restart/30000
  sc start BulwarkAgent
  reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkSession /t REG_SZ /d "\"%ProgramFiles%\Bulwark\bulwark-agent.exe\" --session" /f
  rem Older installs: elevated all-in-one agent.
  schtasks /delete /f /tn "Bulwark Agent" >nul 2>&1
  reg delete HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkAgent /f >nul 2>&1
  rem This session: start the helper without our admin token.
  runas /trustlevel:0x20000 "\"%ProgramFiles%\Bulwark\bulwark-agent.exe\" --session"
)
echo done > C:\bulwark-setup.done
