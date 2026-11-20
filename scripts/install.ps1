#!/usr/bin/env pwsh
# One-command installer for Adus: creates .env, generates secrets, starts the stack.
# Safe to run again: values already in .env are never overwritten.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

Set-Location (Split-Path -Parent $PSScriptRoot)

function Fail($message) {
    [Console]::Error.WriteLine("ERROR: $message")
    exit 1
}

# Value of KEY in .env (empty if missing).
function Get-EnvValue($key) {
    $line = Get-Content .env | Where-Object { $_.StartsWith("$key=") } | Select-Object -First 1
    if ($line) { return $line.Substring($key.Length + 1).Trim() }
    return ''
}

# Set KEY=VALUE in .env, replacing the existing line for KEY. UTF-8 without BOM, LF endings.
function Set-EnvValue($key, $value) {
    $found = $false
    $lines = @(Get-Content .env | ForEach-Object {
        if ($_.StartsWith("$key=")) { $found = $true; "$key=$value" } else { $_ }
    })
    if (-not $found) { $lines += "$key=$value" }
    [IO.File]::WriteAllText("$PWD/.env", (($lines -join "`n") + "`n"))
}

# Run `adus <args>` in a throwaway container; returns the output lines (stdout only).
function Invoke-Adus($adusArgs) {
    $lines = docker compose run --rm --no-deps -T adus @adusArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        $lines | ForEach-Object { [Console]::Error.WriteLine($_) }
        Fail "could not run 'adus $adusArgs'"
    }
    $lines = $lines | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] }
    return ($lines | Where-Object { $_ -and $_.Trim() } | Select-Object -Last 1)
}

# 1. Docker
docker info *> $null
if ($LASTEXITCODE -ne 0) {
    Fail 'Docker is not installed or not running. Install it from https://docs.docker.com/get-docker/ and start it.'
}
$composeVersion = docker compose version --short 2>$null
if ($LASTEXITCODE -ne 0) { Fail 'Docker Compose v2 is missing. See https://docs.docker.com/compose/install/' }
if ([version]($composeVersion -replace '^v', '' -replace '[-+].*$', '') -lt [version]'2.20') {
    Fail "Docker Compose 2.20 or newer is required (found $composeVersion). See https://docs.docker.com/compose/install/"
}

# 2. .env
if (-not (Test-Path .env)) {
    Copy-Item .env.example .env
    Write-Host 'Created .env from .env.example'
}

# 3. Secrets (only when empty)
if (-not (Get-EnvValue 'ADUS_SECRET_KEY')) {
    $secret = Invoke-Adus @('gen-secret')
    if (-not $secret) { Fail "'adus gen-secret' printed nothing" }
    Set-EnvValue 'ADUS_SECRET_KEY' $secret.Trim()
    Write-Host 'Generated ADUS_SECRET_KEY (back up .env: losing it makes stored PATs and LLM keys unreadable)'
}
if (-not (Get-EnvValue 'POSTGRES_PASSWORD')) {
    $password = (Invoke-Adus @('gen-secret')) -replace '[+/=\s]', ''
    if (-not $password) { Fail "'adus gen-secret' printed nothing" }
    Set-EnvValue 'POSTGRES_PASSWORD' $password
    Write-Host 'Generated POSTGRES_PASSWORD'
}

# 4. Public URL
$port = Get-EnvValue 'ADUS_PORT'
if (-not $port) { $port = '8080' }
if (-not (Get-EnvValue 'ADUS_PUBLIC_URL')) {
    $default = "http://localhost:$port"
    $url = $default
    if (-not [Console]::IsInputRedirected) {
        Write-Host 'Public URL: the address your browser and GitHub/Azure DevOps use to reach Adus.'
        Write-Host 'Webhooks cannot reach localhost; use a public https URL (or a tunnel) for real reviews.'
        $answer = Read-Host "ADUS_PUBLIC_URL [$default]"
        if ($answer) { $url = $answer.Trim() }
    }
    Set-EnvValue 'ADUS_PUBLIC_URL' $url
    Write-Host "ADUS_PUBLIC_URL=$url"
}

# 5. Validate
$check = docker compose run --rm --no-deps -T adus check-config 2>&1
if ($LASTEXITCODE -ne 0) {
    $check | ForEach-Object { [Console]::Error.WriteLine($_) }
    Fail 'configuration errors above; fix .env and run this script again'
}
$check | Where-Object { "$_" -like 'WARN*' } | ForEach-Object { Write-Host $_ }

# 6. Start
docker compose pull
if ($LASTEXITCODE -ne 0) { Write-Host 'Could not pull some images; continuing with local images / build' }
docker compose up -d
if ($LASTEXITCODE -ne 0) { Fail 'docker compose up failed' }

# 7. Wait for /healthz
Write-Host 'Waiting for Adus to become healthy (up to 180 s)...'
$ready = $false
foreach ($i in 1..60) {
    try {
        Invoke-WebRequest "http://localhost:$port/healthz" -UseBasicParsing -TimeoutSec 3 | Out-Null
        $ready = $true
        break
    } catch {
        Start-Sleep -Seconds 3
    }
}
if (-not $ready) {
    docker compose logs --tail 50 adus
    Fail 'Adus did not become healthy; see the logs above'
}

# 8. Next steps
$publicUrl = Get-EnvValue 'ADUS_PUBLIC_URL'
Write-Host @"

Adus is running.

  Dashboard:  $publicUrl  (local: http://localhost:$port)

Next steps:
  1. Open $publicUrl/setup and create the first admin account.
  2. Add an AI provider (AI Providers page) with at least one API key.
  3. Choose a default model in Settings.
  4. Add a connection (GitHub or Azure DevOps PAT) and pick repositories.

Check that reviews can run:  docker compose exec adus adus doctor
"@
