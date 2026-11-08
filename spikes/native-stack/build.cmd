@echo off
rem SUPERSEDED by build-144.sh: 14.41 lacks the STL symbols pyke ORT 1.28 needs (LNK1120). Kept for the record.
rem pyke's static ORT (ort-sys 2.0.0-rc.13) needs STL symbols absent from 14.39.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" -vcvars_ver=14.41 >nul || exit /b 1
set "PATH=%USERPROFILE%\.cargo\bin;C:\protoc\bin;%PATH%"
if not defined CARGO_TARGET_DIR set "CARGO_TARGET_DIR=%~dp0target"
cd /d "%~dp0"
where link
cargo %*
