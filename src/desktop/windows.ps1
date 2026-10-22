param([string]$Plan, [string]$Journal, [switch]$Restore)
$ErrorActionPreference = 'Stop'
# Serialize integration across HAPS_HOME values and sessions for this Windows user.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$mutex = New-Object Threading.Mutex($false, ('Global\HapsDesktop-' + $identity))
try { if (!$mutex.WaitOne(30000)) { throw 'Another desktop integration is busy' } }
catch [Threading.AbandonedMutexException] { } # The OS granted ownership after a crashed process.
# The OS releases the mutex on every exit path, including terminating errors.
$p = Get-Content -LiteralPath $Plan -Raw | ConvertFrom-Json
$classes = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\Classes')
function Registry-State($scheme) {
    $key = $classes.OpenSubKey($scheme)
    if (!$key) { return $null }
    try {
        $result = [ordered]@{}
        function Walk($key, $prefix, $result) {
            $result[$prefix + '|#key'] = @('Key', '')
            foreach ($name in $key.GetValueNames()) {
                $result[$prefix + '|' + $name] = @($key.GetValueKind($name).ToString(), $key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames))
            }
            foreach ($name in $key.GetSubKeyNames()) {
                $child = $key.OpenSubKey($name)
                try { Walk $child ($prefix + '\' + $name) $result } finally { $child.Dispose() }
            }
        }
        Walk $key '' $result
        return $result
    } finally { $key.Dispose() }
}
function Expected($entry, $scheme) {
    if (!$entry -or !($entry.schemes -contains $scheme)) { return $null }
    return @{ '|#key' = @('Key', ''); '\shell|#key' = @('Key', ''); '\shell\open|#key' = @('Key', ''); '\shell\open\command|#key' = @('Key', ''); '|' = @('String', ('URL:' + $entry.name)); '|URL Protocol' = @('String', ''); '|HapsOwner' = @('String', $p.owner); '\shell\open\command|' = @('String', ('"' + $entry.command + '" "%1"')) }
}
function Equal-State($a, $b) {
    if ($null -eq $a -or $null -eq $b) { return ($null -eq $a -and $null -eq $b) }
    if ($a.Count -ne $b.Count) { return $false }
    foreach ($k in $a.Keys) {
        if (!$b.Contains($k) -or ($a[$k] | ConvertTo-Json -Compress) -cne ($b[$k] | ConvertTo-Json -Compress)) { return $false }
    }
    return $true
}
function Set-Registry($scheme, $entry) {
    $classes.DeleteSubKeyTree($scheme, $false)
    if (!$entry -or !($entry.schemes -contains $scheme)) { return }
    $key = $classes.CreateSubKey($scheme)
    try {
        $key.SetValue('', ('URL:' + $entry.name))
        $key.SetValue('URL Protocol', '')
        $key.SetValue('HapsOwner', $p.owner)
        $cmd = $key.CreateSubKey('shell\open\command')
        try { $cmd.SetValue('', ('"' + $entry.command + '" "%1"')) } finally { $cmd.Dispose() }
    } finally { $key.Dispose() }
}
$shell = New-Object -ComObject WScript.Shell
function Matches-Link($entry) {
    if (!$entry) { return $false }
    $link = $shell.CreateShortcut($p.shortcut)
    return ($link.TargetPath -ceq $entry.command -and $link.Arguments -ceq '' -and $link.WorkingDirectory -ceq $entry.directory -and $link.IconLocation -ceq ($entry.icon + ',0') -and $link.Description -ceq ($entry.name + ' [' + $p.owner + ']') -and $link.Hotkey -ceq '' -and $link.WindowStyle -eq 1)
}
function Set-Link($entry) {
    if (!$entry) { if (Test-Path -LiteralPath $p.shortcut) { Remove-Item -LiteralPath $p.shortcut }; return }
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($p.shortcut)) | Out-Null
    $link = $shell.CreateShortcut($p.shortcut)
    $link.TargetPath = $entry.command
    $link.Arguments = ''
    $link.WorkingDirectory = $entry.directory
    $link.IconLocation = $entry.icon + ',0'
    $link.Description = $entry.name + ' [' + $p.owner + ']'
    $link.Hotkey = ''
    $link.WindowStyle = 1
    $link.Save()
}
$schemes = @(@($p.old.schemes) + @($p.new.schemes) | Where-Object { $_ } | Sort-Object -Unique)
if ($Restore) {
    $j = Get-Content -LiteralPath $Journal -Raw | ConvertFrom-Json
    foreach ($scheme in $schemes) {
        $current = Registry-State $scheme
        if ($null -ne $current -and !(Equal-State $current (Expected $p.new $scheme)) -and !(Equal-State $current (Expected $p.old $scheme))) { throw "URI handler changed during transaction: $scheme" }
    }
    if ((Test-Path -LiteralPath $p.shortcut) -and !(Matches-Link $p.old) -and !(Matches-Link $p.new)) { throw 'Shortcut changed during transaction' }
    foreach ($scheme in $schemes) { if ($j.present -contains $scheme) { Set-Registry $scheme $p.old } else { Set-Registry $scheme $null } }
    if ($null -eq $j.shortcut) { Set-Link $null } else { [IO.File]::WriteAllBytes($p.shortcut, [Convert]::FromBase64String($j.shortcut)) }
    exit 0
}
# Validate everything before the first mutation. Never claim a machine-wide handler.
$present = @()
foreach ($scheme in $schemes) {
    $current = Registry-State $scheme
    if ($null -ne $current) {
        if (!(Equal-State $current (Expected $p.old $scheme))) { throw "Existing URI handler is not owned by this installation: $scheme" }
        $present += $scheme
    } else {
        $machine = [Microsoft.Win32.Registry]::ClassesRoot.OpenSubKey($scheme)
        if ($machine) { $machine.Dispose(); throw "Existing system URI handler: $scheme" }
    }
}
$bytes = $null
if (Test-Path -LiteralPath $p.shortcut) {
    if (!(Matches-Link $p.old)) { throw 'Existing shortcut is not owned by this installation' }
    $bytes = [Convert]::ToBase64String([IO.File]::ReadAllBytes($p.shortcut))
}
@{ present = $present; shortcut = $bytes } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Journal -Encoding UTF8
try {
    foreach ($scheme in $schemes) { Set-Registry $scheme $p.new }
    Set-Link $p.new
    foreach ($scheme in $schemes) {
        if (!(Equal-State (Registry-State $scheme) (Expected $p.new $scheme))) { throw "URI registration verification failed: $scheme" }
    }
    if ($p.new) { if (!(Matches-Link $p.new)) { throw 'Shortcut verification failed' } }
    elseif (Test-Path -LiteralPath $p.shortcut) { throw 'Shortcut removal failed' }
} catch {
    $original = $_
    foreach ($scheme in $schemes) { if ($present -contains $scheme) { Set-Registry $scheme $p.old } else { Set-Registry $scheme $null } }
    if ($null -eq $bytes) { Set-Link $null } else { [IO.File]::WriteAllBytes($p.shortcut, [Convert]::FromBase64String($bytes)) }
    throw $original
}
