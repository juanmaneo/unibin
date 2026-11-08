# unibin hardware test for Windows 11 (x64 and arm64). Run it from the kit:
#
#   powershell -ExecutionPolicy Bypass -File .\run.ps1          (add -NoSign to skip signing)
#
# It runs probe.com and probe-x86_64.com through the PE launcher, checks what
# the probe reports, signs a copy (probe-signed.com) with a throwaway
# self-signed certificate, and writes everything to hwtest-Windows-<arch>.txt.
# Send that file back. Exit status: 0 if every check passed.
#
# Works with the stock Windows PowerShell 5.1. Keep this file ASCII: 5.1
# reads BOM-less scripts in the ANSI code page.

param([switch]$NoSign)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$kit = $PSScriptRoot
Set-Location $kit

# The machine's own architecture. $env:PROCESSOR_ARCHITECTURE describes this
# PowerShell process, which may itself be emulated.
$envKey = 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
$nativeArch = (Get-ItemProperty $envKey).PROCESSOR_ARCHITECTURE
$arch = switch ($nativeArch) { 'ARM64' { 'aarch64' } 'AMD64' { 'x86_64' } default { $nativeArch } }
$hostKey = "Windows-$arch"
$nativeMachine = switch ($arch) { 'aarch64' { '0xAA64' } default { '0x8664' } }
$report = Join-Path $kit "hwtest-$hostKey.txt"
Set-Content -Path $report -Value "unibin hardware test, host $hostKey" -Encoding UTF8
$script:pass = 0; $script:fail = 0; $script:skip = 0

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("unibin-hwtest." + [guid]::NewGuid().ToString('N').Substring(0, 8))
$tmp = Join-Path $work 'tmp'
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

function Log([string]$Line) {
    Write-Host $Line
    Add-Content -Path $report -Value $Line -Encoding UTF8
}
function Ok([string]$Name) { $script:pass++; Log "PASS $Name" }
function Bad([string]$Name, [string]$Why) { $script:fail++; Log "FAIL ${Name}: $Why" }
function Skipped([string]$Name, [string]$Why) { $script:skip++; Log "SKIP ${Name}: $Why" }

# Quote one argument the way the MSVC CRT and Rust's std parse it.
function ConvertTo-CmdArg([string]$Arg) {
    if ($Arg.Length -gt 0 -and $Arg -notmatch '[\s"]') { return $Arg }
    $out = '"'; $slashes = 0
    foreach ($c in $Arg.ToCharArray()) {
        if ($c -eq '\') { $slashes++; continue }
        if ($c -eq '"') { $out += ('\' * (2 * $slashes + 1)) + '"' }
        else { $out += ('\' * $slashes) + $c }
        $slashes = 0
    }
    return $out + ('\' * (2 * $slashes)) + '"'
}

# Run Exe with exactly Args (quoted by us, not by PowerShell, whose 5.1
# quoting drops empty arguments and mangles embedded quotes). Returns the
# exit code, stdout, stderr and the parsed key=value facts.
function Invoke-Probe([string]$Name, [string]$Exe, [string[]]$Argv, [string]$StdIn = '', [string]$Temp = $tmp) {
    $line = ($Argv | ForEach-Object { ConvertTo-CmdArg $_ }) -join ' '
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = $line
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $psi.EnvironmentVariables['TMP'] = $Temp
    $psi.EnvironmentVariables['TEMP'] = $Temp
    $r = [ordered]@{ Code = $null; Out = ''; Err = ''; Facts = @{} }
    try {
        $p = [System.Diagnostics.Process]::Start($psi)
        $outTask = $p.StandardOutput.ReadToEndAsync()
        $errTask = $p.StandardError.ReadToEndAsync()
        $p.StandardInput.Write($StdIn)
        $p.StandardInput.Close()
        $p.WaitForExit()
        $r.Code = $p.ExitCode; $r.Out = $outTask.Result; $r.Err = $errTask.Result
    } catch {
        $r.Err = "cannot start: $($_.Exception.Message)"
    }
    foreach ($l in ($r.Out -split "`r?`n")) {
        if ($l -match '^([A-Za-z][\w.]*)=(.*)$' -and -not $r.Facts.Contains($Matches[1])) {
            $r.Facts[$Matches[1]] = $Matches[2]
        }
    }
    $text = "--- ${Name}: $Exe $line`r`n    rc=$($r.Code)"
    foreach ($l in ($r.Out.TrimEnd() -split "`r?`n")) { if ($l) { $text += "`r`n    out| $l" } }
    foreach ($l in ($r.Err.TrimEnd() -split "`r?`n")) { if ($l) { $text += "`r`n    err| $l" } }
    Add-Content -Path $report -Value $text -Encoding UTF8
    return $r
}

# The last run exited Code from slice Slice and received exactly Args.
function Test-Probe([string]$Name, $R, [int]$Code, [string]$Slice, [string[]]$Argv) {
    $why = @()
    if ($R.Code -ne $Code) { $why += "exit $($R.Code), want $Code" }
    $got = $R.Facts['slice']
    if ($got -ne $Slice) { $why += "slice '$got', want '$Slice'" }
    $got = $R.Facts['argc']
    if ($got -ne [string]$Argv.Count) { $why += "argc '$got', want $($Argv.Count)" }
    for ($i = 0; $i -lt $Argv.Count; $i++) {
        $got = $R.Facts["arg$($i + 1)"]
        if ($got -cne $Argv[$i]) { $why += "arg$($i + 1) '$got', want '$($Argv[$i])'" }
    }
    if ($why.Count -eq 0) { Ok $Name } else { Bad $Name ($why -join '; ') }
}

function Test-Fact([string]$Name, $R, [string]$Key, [string]$Want) {
    $got = $R.Facts[$Key]
    if ($got -eq $Want) { Ok $Name } else { Bad $Name "$Key '$got', want '$Want'" }
}

# The launcher deletes its extraction directory before it exits.
function Test-Cleanup([string]$Name, [string]$Dir = $tmp) {
    $left = @(Get-ChildItem -Force -LiteralPath $Dir | ForEach-Object { $_.Name })
    if ($left.Count -eq 0) { Ok $Name } else { Bad $Name "left behind in ${Dir}: $($left -join ', ')" }
}

# System facts that explain most launch failures.
$os = Get-CimInstance Win32_OperatingSystem
$info = @(
    '== system',
    "$($os.Caption) $($os.Version) build $($os.BuildNumber)",
    "native arch: $nativeArch; this PowerShell: $env:PROCESSOR_ARCHITECTURE $($PSVersionTable.PSVersion)"
)
$sac = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -ErrorAction SilentlyContinue)
if ($sac -and ($sac.PSObject.Properties.Name -contains 'VerifiedAndReputablePolicyState')) {
    $state = switch ($sac.VerifiedAndReputablePolicyState) { 0 { 'off' } 1 { 'ON (may block unsigned slices)' } 2 { 'evaluation' } default { "$_" } }
    $info += "Smart App Control: $state"
}
foreach ($f in 'probe.com', 'probe-x86_64.com') {
    $path = Join-Path $kit $f
    if (-not (Test-Path -LiteralPath $path)) { Log "missing ${path}: run this from the unpacked kit"; exit 1 }
    $item = Get-Item -LiteralPath $path
    $motw = Get-Item -LiteralPath $path -Stream Zone.Identifier -ErrorAction SilentlyContinue
    $info += "$f $($item.Length) bytes" + $(if ($motw) { ' (Mark of the Web: Unblock-File it if Windows blocks it)' } else { '' })
}
Add-Content -Path $report -Value (($info + '== checks') -join "`r`n") -Encoding UTF8

$probe = Join-Path $kit 'probe.com'
$fallback = Join-Path $kit 'probe-x86_64.com'

try {
    # 1. The launcher picks this machine's native slice. On arm64 that means
    # its IsWow64Process2 call saw ARM64 although the launcher is x86_64.
    $a = @('a b', '', 'q"uote', 'trail\', 'back\\"slash', '*.txt', '%PATH%', ('u' + [char]0x00FC + [char]0x20AC))
    $r = Invoke-Probe 'native' $probe (@('--exit', '42') + $a)
    Test-Probe 'native' $r 42 $hostKey $a
    Test-Fact 'native-not-translated' $r 'translated' '0'
    Test-Fact 'native-machine' $r 'win.native_machine' $nativeMachine

    $r = Invoke-Probe 'exit-1000' $probe @('--exit', '1000')
    Test-Probe 'exit-1000' $r 1000 $hostKey @()
    # STATUS_CONTROL_C_EXIT, what a child killed by Ctrl-C returns.
    $r = Invoke-Probe 'exit-ntstatus' $probe @('--exit', '-1073741510')
    Test-Probe 'exit-ntstatus' $r (-1073741510) $hostKey @()

    $r = Invoke-Probe 'stdin' $probe @('--stdin') -StdIn "hello`r`n"
    Test-Probe 'stdin' $r 0 $hostKey @()
    Test-Fact 'stdin-content' $r 'stdin' 'hello'
    Test-Cleanup 'temp-dir-cleanup'

    $odd = Join-Path $work ('dir with spaces ' + [char]0x00FC)
    $oddTmp = Join-Path $odd 'tmp dir'
    New-Item -ItemType Directory -Path $oddTmp -Force | Out-Null
    Copy-Item -LiteralPath $probe -Destination (Join-Path $odd 'probe.com')
    $r = Invoke-Probe 'odd-paths' (Join-Path $odd 'probe.com') @('--exit', '6', 'z') -Temp $oddTmp
    Test-Probe 'odd-paths' $r 6 $hostKey @('z')
    Test-Cleanup 'odd-paths-cleanup' $oddTmp

    # 2. x86_64 slice only: native on x64, the Prism fallback on arm64. The
    # probe makes the launcher's own IsWow64Process2 call from inside Prism.
    $r = Invoke-Probe 'fallback' $fallback @('--exit', '7', 'a b')
    if ($arch -eq 'aarch64') {
        Test-Probe 'fallback-prism' $r 7 'Windows-x86_64' @('a b')
        Test-Fact 'fallback-prism-translated' $r 'translated' '1'
        Test-Fact 'prism-native-machine' $r 'win.native_machine' '0xAA64'
    } else {
        Test-Probe 'fallback-native' $r 7 $hostKey @('a b')
    }
    Test-Cleanup 'fallback-cleanup'

    # 3. Authenticode-sign a copy after packing, then run it. Copy
    # probe-signed.com back to a Unix host: run.sh checks it there too.
    if ($NoSign) {
        Skipped 'signed' '-NoSign'
    } else {
        $signed = Join-Path $kit 'probe-signed.com'
        Copy-Item -LiteralPath $probe -Destination $signed -Force
        $cert = $null
        try {
            $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=unibin hwtest (throwaway)' `
                -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddDays(1)
            $s = Set-AuthenticodeSignature -FilePath $signed -Certificate $cert -HashAlgorithm SHA256
            $check = Get-AuthenticodeSignature -FilePath $signed
            $size = (Get-Item -LiteralPath $signed).Length
            Add-Content -Path $report -Encoding UTF8 -Value (
                "--- sign: Set-AuthenticodeSignature status=$($s.Status) ($($s.StatusMessage))`r`n" +
                "    verify: status=$($check.Status) signer=$($check.SignerCertificate.Thumbprint) size=$size")
            # Self-signed, so the chain is untrusted (UnknownError); the
            # signature itself must be there and be ours.
            if ($check.SignerCertificate -and $check.SignerCertificate.Thumbprint -eq $cert.Thumbprint -and
                $check.Status -ne 'HashMismatch' -and $check.Status -ne 'NotSigned') {
                Ok 'sign'
            } else {
                Bad 'sign' "status $($check.Status): $($check.StatusMessage)"
            }
            $r = Invoke-Probe 'signed' $signed @('--exit', '42', 'a b')
            Test-Probe 'signed' $r 42 $hostKey @('a b')
            Test-Cleanup 'signed-cleanup'
        } catch {
            Bad 'sign' $_.Exception.Message
        } finally {
            if ($cert) { Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($cert.Thumbprint)" -ErrorAction SilentlyContinue }
        }
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Log "RESULT $hostKey pass=$($script:pass) fail=$($script:fail) skip=$($script:skip) (report: $report)"
if ($script:fail -ne 0) { exit 1 }
