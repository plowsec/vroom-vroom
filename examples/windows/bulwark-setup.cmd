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
rem Agent (clipboard, time sync, resolution, shutdown), autostart at logon.
if exist "%~dp0bulwark-agent.exe" (
  mkdir "%ProgramFiles%\Bulwark" 2>nul
  copy /y "%~dp0bulwark-agent.exe" "%ProgramFiles%\Bulwark\"
  reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v BulwarkAgent /t REG_SZ /d "\"%ProgramFiles%\Bulwark\bulwark-agent.exe\"" /f
)
echo done > C:\bulwark-setup.done
