@echo off
rem bulwark guest tools for Windows ARM64: virtio drivers + agent.
rem Looks for the virtio-win media on any drive letter.
for %%d in (D E F G H I J K) do (
  if exist %%d:\viogpudo\w11\ARM64 (
    for %%x in (viogpudo NetKVM vioserial vioinput viorng viostor) do (
      pnputil /add-driver %%d:\%%x\w11\ARM64\*.inf /install
    )
  )
)
rem Agent (clipboard, time sync, resolution, shutdown), started at logon in
rem the user's session. Elevated: the VirtIO serial port only admits
rem Administrators and SYSTEM. (Standard users need a service + session
rem helper split; not done yet.)
if exist "%~dp0bulwark-agent.exe" (
  mkdir "%ProgramFiles%\Bulwark" 2>nul
  copy /y "%~dp0bulwark-agent.exe" "%ProgramFiles%\Bulwark\"
  schtasks /create /f /tn "Bulwark Agent" /tr "\"%ProgramFiles%\Bulwark\bulwark-agent.exe\"" /sc onlogon /rl highest /it
  reg delete HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkAgent /f 2>nul
)
echo done > C:\bulwark-setup.done
