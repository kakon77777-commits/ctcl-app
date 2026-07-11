@echo off
title CTCL Temporal Port - Local Preview
cd /d "%~dp0"
echo Starting CTCL preview, your browser will open automatically...
target\release\ctcl.exe serve
pause
