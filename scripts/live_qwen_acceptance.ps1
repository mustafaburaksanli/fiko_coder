# Human-approved live-model acceptance measurement; never runs Ollama or grants approvals.
param(
    [Parameter(Mandatory=$true)][ValidateSet('prepare','evaluate')][string]$Mode,
    [Parameter(Mandatory=$true)][string]$FixtureDir
)
$ErrorActionPreference = 'Stop'
$path = [System.IO.Path]::GetFullPath($FixtureDir).TrimEnd([IO.Path]::DirectorySeparatorChar)
$parent = [System.IO.Path]::GetDirectoryName($path)
$name = [System.IO.Path]::GetFileName($path)
if ($name -notmatch '^fiko_live_[a-zA-Z0-9_-]+$') {
    throw 'FixtureDir must end in a directory named fiko_live_<id> (letters, digits, _ and - only).'
}
$baselinePath = Join-Path $parent ($name + '.baseline.json')
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Invoke-FixtureTests([string]$root) {
    Push-Location $root
    try {
        $output = (& cargo test --offline 2>&1 | Out-String)
        return @{ output = $output; code = $LASTEXITCODE }
    } finally { Pop-Location }
}

if ($Mode -eq 'prepare') {
    if ((Test-Path -LiteralPath $path) -or (Test-Path -LiteralPath $baselinePath)) {
        throw 'Refusing to overwrite an existing fixture or baseline.'
    }
    & cargo new --lib --vcs none -- $path
    if ($LASTEXITCODE -ne 0) { throw 'cargo new failed' }
    [IO.File]::WriteAllText((Join-Path $path 'src/lib.rs'), ("pub mod math;" + [char]10), $utf8)
    $math = @'
pub fn double(n: i32) -> i32 { n * 2 }
pub fn triple(n: i32) -> i32 { n * 3 }
pub fn quadruple(n: i32) -> i32 { n * 4 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn test_double() { assert_eq!(double(3), 6); }
    #[test] fn test_triple() { assert_eq!(triple(3), 9); }
    #[test] fn test_quadruple() { assert_eq!(quadruple(3), 12); }
}
'@
    [IO.File]::WriteAllText((Join-Path $path 'src/math.rs'), ($math + [char]10), $utf8)
    $run = Invoke-FixtureTests $path
    if ($run.code -ne 0 -or $run.output -notmatch 'test result: ok\. 3 passed; 0 failed;') {
        throw "Baseline Cargo tests failed: $($run.output)"
    }
    $baseline = [ordered]@{
        schema = 1
        fixture = $path
        protected_sha256 = (Get-FileHash -LiteralPath (Join-Path $path 'src/lib.rs') -Algorithm SHA256).Hash
        manifest_sha256 = (Get-FileHash -LiteralPath (Join-Path $path 'Cargo.toml') -Algorithm SHA256).Hash
    }
    [IO.File]::WriteAllText($baselinePath, ($baseline | ConvertTo-Json), $utf8)
    Write-Output "Baseline passed (3 tests). Fixture: $path"
    Write-Output "Run the FIKO CODER agent manually here. Review the exact diff and explicitly approve only appropriate changes."
    exit 0
}

if (-not (Test-Path -LiteralPath $baselinePath -PathType Leaf)) {
    throw 'Missing baseline; run prepare first.'
}
$baseline = Get-Content -LiteralPath $baselinePath -Raw | ConvertFrom-Json
if ($baseline.schema -ne 1 -or $baseline.fixture -ne $path) {
    throw 'Baseline identity mismatch.'
}
$lib = Join-Path $path 'src/lib.rs'
$math = Join-Path $path 'src/math.rs'
$manifest = Join-Path $path 'Cargo.toml'
if (-not (Test-Path -LiteralPath $lib -PathType Leaf) -or
    -not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
    throw 'Required Cargo manifest or protected source missing.'
}
$source = if (Test-Path -LiteralPath $math -PathType Leaf) {
    Get-Content -LiteralPath $math -Raw
} else { '' }
$sourcePaths = @(Get-ChildItem -LiteralPath (Join-Path $path 'src') -Recurse -File -Filter '*.rs' |
    ForEach-Object { $_.FullName.Substring($path.Length + 1).Replace('\','/') })
$checks = [ordered]@{
    protected_source_unchanged = ((Get-FileHash -LiteralPath $lib -Algorithm SHA256).Hash -eq $baseline.protected_sha256)
    manifest_unchanged = ((Get-FileHash -LiteralPath $manifest -Algorithm SHA256).Hash -eq $baseline.manifest_sha256)
    only_expected_rust_sources = ($sourcePaths.Count -eq 2 -and $sourcePaths -contains 'src/lib.rs' -and $sourcePaths -contains 'src/math.rs')
    quintuple_function_present = ($source -match '(?m)^\s*pub\s+fn\s+quintuple\s*\(')
    quintuple_test_present = ($source -match '(?m)^\s*fn\s+test_quintuple\s*\(')
    existing_functions_present = (($source -match '(?m)^\s*pub\s+fn\s+double\s*\(') -and ($source -match '(?m)^\s*pub\s+fn\s+triple\s*\(') -and ($source -match '(?m)^\s*pub\s+fn\s+quadruple\s*\('))
    existing_tests_present = (($source -match '(?m)^\s*fn\s+test_double\s*\(') -and ($source -match '(?m)^\s*fn\s+test_triple\s*\(') -and ($source -match '(?m)^\s*fn\s+test_quadruple\s*\('))
    pending_marker_absent = (-not (Test-Path -LiteralPath (Join-Path $path '.fiko_backups/workspace.pending.json')))
}
$run = Invoke-FixtureTests $path
$checks['cargo_exit_zero'] = ($run.code -eq 0)
$checks['four_tests_pass'] = ($run.output -match 'test result: ok\. 4 passed; 0 failed;')
$passed = (@($checks.Values | Where-Object { -not $_ }).Count -eq 0)
$result = [ordered]@{
    schema = 1
    fixture = $path
    measured_utc = [DateTime]::UtcNow.ToString('o')
    pass = $passed
    checks = $checks
    cargo_exit_code = $run.code
}
$reportPath = Join-Path $parent ($name + '.result.json')
[IO.File]::WriteAllText($reportPath, ($result | ConvertTo-Json -Depth 5), $utf8)
Write-Output ($result | ConvertTo-Json -Depth 5)
Write-Output "Report: $reportPath"
if (-not $passed) { exit 1 }
