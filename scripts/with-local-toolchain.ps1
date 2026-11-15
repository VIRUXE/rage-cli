# Run Cargo against the adjacent toolchain checkouts without changing manifests
# or leaving a locally patched lockfile behind. No shell command strings.
$ErrorActionPreference = 'Stop'
# Deliberately use no PowerShell parameter binder: Cargo's -i/-v flags would
# otherwise bind to PowerShell's common parameters or the toolchain root.
$ToolchainRoot = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
$cargoArguments = @($args)
if ($cargoArguments.Count -gt 0 -and $cargoArguments[0] -eq '-ToolchainRoot') {
    if ($cargoArguments.Count -lt 2) { throw '-ToolchainRoot needs a path' }
    $ToolchainRoot = $cargoArguments[1]
    $cargoArguments = @($cargoArguments | Select-Object -Skip 2)
}
if ($cargoArguments.Count -gt 0 -and $cargoArguments[0] -eq '--') {
    $cargoArguments = @($cargoArguments | Select-Object -Skip 1)
}
if ($cargoArguments.Count -eq 0) { $cargoArguments = @('test') }

$repoRoot = Split-Path $PSScriptRoot -Parent
$lockPath = Join-Path $repoRoot 'Cargo.lock'
$lockExisted = Test-Path -LiteralPath $lockPath
$originalLock = if ($lockExisted) { [IO.File]::ReadAllBytes($lockPath) } else { $null }
$configPath = Join-Path ([IO.Path]::GetTempPath()) ('rage-local-' + [Guid]::NewGuid().ToString('N') + '.toml')
$exitCode = 1
try {
    $paths = @{}
    foreach ($package in @('rpf-archive', 'rage-formats', 'rage-render')) {
        $directory = if ($package -eq 'rpf-archive') { 'rpf-archive-rs' } else { $package }
        $resolved = (Resolve-Path -LiteralPath (Join-Path $ToolchainRoot $directory)).Path
        if (-not (Test-Path -LiteralPath (Join-Path $resolved 'Cargo.toml'))) {
            throw "Missing $package Cargo.toml under $resolved"
        }
        # JSON string quoting also produces a valid TOML basic string here.
        $paths[$package] = ConvertTo-Json -InputObject $resolved.Replace('\', '/') -Compress
    }
    $config = @"
[patch.crates-io]
rpf-archive = { path = $($paths['rpf-archive']) }
rage-formats = { path = $($paths['rage-formats']) }
rage-render = { path = $($paths['rage-render']) }

[patch."https://github.com/VIRUXE/rage-render"]
rage-render = { path = $($paths['rage-render']) }
"@
    [IO.File]::WriteAllText($configPath, $config, [Text.UTF8Encoding]::new($false))
    Push-Location $repoRoot
    try {
        & cargo --config $configPath @cargoArguments
        $exitCode = $LASTEXITCODE
    } finally { Pop-Location }
} finally {
    if ($lockExisted) { [IO.File]::WriteAllBytes($lockPath, $originalLock) }
    elseif (Test-Path -LiteralPath $lockPath) { Remove-Item -LiteralPath $lockPath }
    if (Test-Path -LiteralPath $configPath) { Remove-Item -LiteralPath $configPath }
}
exit $exitCode
