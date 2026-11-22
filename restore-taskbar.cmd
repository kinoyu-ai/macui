@echo off
setlocal
set "dock=%~dp0macui.exe"
if not exist "%dock%" set "dock=%~dp0target\release\macui.exe"
if not exist "%dock%" exit /b 1
start "" /wait "%dock%" --restore
