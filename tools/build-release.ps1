# Build the 32-bit game host, then embed that exact binary in the launcher.
[CmdletBinding()]
param(
    [string]$OutputPath,
    [string]$LoaderTargetDir,
    [string]$LoaderFeatures,
    [string]$LauncherFeatures,
    [string]$InstallerCompiler,
    [switch]$Offline
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
# Path.GetFullPath(path, base) is .NET Core only; Windows PowerShell 5.1 runs this too.
function Get-FullPath([string]$Path, [string]$Base) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $Base $Path))
}
$originalTarget = $env:CARGO_TARGET_DIR
$originalLoader = $env:CRYSTAL_LOADER_EXE
$originalEncodedRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
$appTarget = if ($originalTarget) { Get-FullPath $originalTarget $root } else { Join-Path $root 'target' }
$loaderTarget = if ($LoaderTargetDir) { Get-FullPath $LoaderTargetDir $root }
    elseif ($originalTarget) { $appTarget } else { Join-Path $root 'crystal-loader\target' }
$out = if ($OutputPath) { Get-FullPath $OutputPath $root } else { Join-Path $appTarget 'Crystal-Launcher.exe' }
if ($InstallerCompiler -and [IO.Path]::GetFileName($out) -cne 'Crystal-Launcher.exe') {
    throw 'Installer builds require an OutputPath ending in Crystal-Launcher.exe.'
}
[string[]]$offlineArgs = @()
if ($Offline) { $offlineArgs = @('--offline') }
[string[]]$loaderFeatureArgs = @()
if ($LoaderFeatures) { $loaderFeatureArgs = @('--features', $LoaderFeatures) }
[string[]]$launcherFeatureArgs = @('--features', ('elevated-ui' + $(if ($LauncherFeatures) { ',' + $LauncherFeatures } else { '' })))

try {
    # Keep local user and build paths out of distributed executables.
    [string[]]$buildRustFlags = @()
    if ($originalEncodedRustFlags) { $buildRustFlags = $originalEncodedRustFlags.Split([char]31) }
    elseif ($env:RUSTFLAGS) { $buildRustFlags = $env:RUSTFLAGS -split '\s+' }
    $buildUserProfile = [Environment]::GetFolderPath('UserProfile')
    if ($buildUserProfile) { $buildRustFlags += "--remap-path-prefix=$buildUserProfile=build" }
    $buildRustFlags += "--remap-path-prefix=$root=crystal-launcher"
    $buildRustFlags += "--remap-path-prefix=$appTarget=target"
    $buildRustFlags += "--remap-path-prefix=$loaderTarget=host-target"
    # CARGO_ENCODED_RUSTFLAGS replaces every rustflags entry in .cargo/config.toml, so the
    # loader's own flag from crystal-loader/.cargo/config.toml is repeated here: a static CRT,
    # so the host starts without the VC++ redistributable. (/LARGEADDRESSAWARE comes from
    # crystal-loader/build.rs as a link argument and is not affected.)
    $env:CARGO_ENCODED_RUSTFLAGS = [string]::Join([char]31, $buildRustFlags + @('-C', 'target-feature=+crt-static'))
    $env:CARGO_TARGET_DIR = $loaderTarget
    Push-Location (Join-Path $root 'crystal-loader')
    try {
        cargo build --locked --release --target i686-pc-windows-msvc --bin pol @offlineArgs @loaderFeatureArgs
        if ($LASTEXITCODE -ne 0) { throw "Loader build failed ($LASTEXITCODE)." }
    } finally { Pop-Location }
    $env:CARGO_ENCODED_RUSTFLAGS = [string]::Join([char]31, $buildRustFlags)
    $loader = Join-Path $loaderTarget 'i686-pc-windows-msvc\release\pol.exe'
    if (-not (Test-Path -LiteralPath $loader -PathType Leaf)) { throw "Loader missing after build: $loader" }
    # The shipped host must be large-address-aware (4 GB for FFXI) and import no dynamic CRT.
    $hostBytes = [IO.File]::ReadAllBytes($loader)
    $nt = [BitConverter]::ToInt32($hostBytes, 0x3C)
    if (([BitConverter]::ToUInt16($hostBytes, $nt + 22) -band 0x20) -eq 0) { throw "Loader is not large-address-aware: $loader" }
    $hostText = [Text.Encoding]::ASCII.GetString($hostBytes)
    foreach ($crt in @('VCRUNTIME140', 'api-ms-win-crt-', 'ucrtbase')) {
        if ($hostText.IndexOf($crt, [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "Loader imports the dynamic CRT ($crt): $loader" }
    }

    $env:CARGO_TARGET_DIR = $appTarget
    $env:CRYSTAL_LOADER_EXE = $loader
    Push-Location $root
    try {
        cargo build --locked --release @launcherFeatureArgs @offlineArgs
        if ($LASTEXITCODE -ne 0) { throw "Launcher build failed ($LASTEXITCODE)." }
    } finally { Pop-Location }
    $exe = Join-Path $appTarget 'release\xi-launcher.exe'
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Launcher missing after build: $exe" }
    $outputDir = Split-Path -Parent $out
    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
    if (-not [string]::Equals($exe, $out, [StringComparison]::OrdinalIgnoreCase)) {
        Copy-Item -LiteralPath $exe -Destination $out -Force
    }
    $hostDir = Join-Path $outputDir 'crystal'
    New-Item -ItemType Directory -Path $hostDir -Force | Out-Null
    $installedHost = Join-Path $hostDir 'pol.exe'
    if (-not [string]::Equals($loader, $installedHost, [StringComparison]::OrdinalIgnoreCase)) {
        Copy-Item -LiteralPath $loader -Destination $installedHost -Force
    }
    foreach ($notice in @('README.md', 'LICENSE', 'THIRD_PARTY.md')) {
        $sourceNotice = Join-Path $root $notice
        $outputNotice = Join-Path $outputDir $notice
        if (-not [string]::Equals($sourceNotice, $outputNotice, [StringComparison]::OrdinalIgnoreCase)) {
            Copy-Item -LiteralPath $sourceNotice -Destination $outputNotice -Force
        }
    }
    python (Join-Path $PSScriptRoot 'collect-licenses.py') --output (Join-Path $outputDir 'THIRD_PARTY_LICENSES.txt') @offlineArgs
    if ($LASTEXITCODE -ne 0) { throw "Could not collect dependency license texts ($LASTEXITCODE)." }
    if ($InstallerCompiler) {
        $compiler = (Resolve-Path -LiteralPath $InstallerCompiler).Path
        $versionMatch = [regex]::Match((Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw), '(?m)^version\s*=\s*"([^"]+)"')
        if (-not $versionMatch.Success) { throw 'Launcher package version is missing.' }
        $version = $versionMatch.Groups[1].Value
        $setup = Join-Path $outputDir "Crystal-Launcher-$version-Setup.exe"
        & $compiler /V2 "/DPACKAGE_DIR=$outputDir" "/DOUTPUT_FILE=$setup" "/DAPP_VERSION=$version" (Join-Path $PSScriptRoot 'installer.nsi')
        if ($LASTEXITCODE -ne 0) { throw "Installer compilation failed ($LASTEXITCODE)." }
        Write-Host "Wrote $setup"
    }
    Write-Host "Wrote $out ($((Get-Item -LiteralPath $out).Length) bytes)"
} finally {
    $env:CARGO_TARGET_DIR = $originalTarget
    $env:CRYSTAL_LOADER_EXE = $originalLoader
    $env:CARGO_ENCODED_RUSTFLAGS = $originalEncodedRustFlags
}
