# Belt installer for Windows
# Usage: irm https://raw.githubusercontent.com/kys0213/belt/main/install.ps1 | iex
#
# Environment variables:
#   BELT_VERSION      - Version to install (default: latest)
#   BELT_INSTALL_DIR  - Installation directory (default: $HOME\.belt\bin)

$ErrorActionPreference = "Stop"

$Repo = "kys0213/belt"
$GitHubApi = "https://api.github.com"
$GitHubRelease = "https://github.com/$Repo/releases"

function Write-Installer {
    param([string]$Message)
    Write-Host "belt-installer: $Message"
}

function Write-InstallerError {
    param([string]$Message)
    Write-Host "belt-installer: ERROR: $Message" -ForegroundColor Red
    exit 1
}

# --- Detect architecture ---

function Get-MachineArch {
    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        "X64" { return "x86_64" }
        default { Write-InstallerError "unsupported architecture: $arch (only x86_64 is supported on Windows)" }
    }
}

# --- Resolve version ---

function Resolve-Version {
    $version = $env:BELT_VERSION
    if ($version) {
        return $version
    }

    Write-Installer "fetching latest release version..."
    $url = "$GitHubApi/repos/$Repo/releases/latest"

    try {
        $response = Invoke-RestMethod -Uri $url -UseBasicParsing
        $tag = $response.tag_name
        if (-not $tag) {
            Write-InstallerError "could not determine latest version from GitHub API"
        }
        return $tag
    }
    catch {
        Write-InstallerError "failed to fetch latest release info: $_"
    }
}

# --- Main ---

function Install-Belt {
    $arch = Get-MachineArch
    Write-Installer "detected platform: windows $arch"

    $version = Resolve-Version
    Write-Installer "installing belt $version"

    $installDir = $env:BELT_INSTALL_DIR
    if (-not $installDir) {
        $installDir = Join-Path $HOME ".belt\bin"
    }

    $asset = "belt-x86_64-pc-windows-msvc.zip"
    $url = "$GitHubRelease/download/$version/$asset"

    # Create install directory
    if (-not (Test-Path $installDir)) {
        try {
            New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        }
        catch {
            Write-InstallerError "failed to create install directory: $installDir ($_)"
        }
    }

    # Download to temp directory
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "belt-install-$([System.Guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

    try {
        $tmpFile = Join-Path $tmpDir $asset

        Write-Installer "downloading $url..."
        try {
            Invoke-WebRequest -Uri $url -OutFile $tmpFile -UseBasicParsing
        }
        catch {
            Write-InstallerError "failed to download $url ($_)"
        }

        # Extract
        Write-Installer "extracting to $installDir..."
        try {
            Expand-Archive -Path $tmpFile -DestinationPath $tmpDir -Force
        }
        catch {
            Write-InstallerError "failed to extract archive ($_)"
        }

        # Find and install belt.exe
        $beltExe = $null
        $candidates = @(
            (Join-Path $tmpDir "belt.exe"),
            (Join-Path $tmpDir "belt-x86_64-pc-windows-msvc\belt.exe")
        )

        foreach ($candidate in $candidates) {
            if (Test-Path $candidate) {
                $beltExe = $candidate
                break
            }
        }

        if (-not $beltExe) {
            # Search recursively
            $found = Get-ChildItem -Path $tmpDir -Filter "belt.exe" -Recurse -File | Select-Object -First 1
            if ($found) {
                $beltExe = $found.FullName
            }
            else {
                Write-InstallerError "could not find 'belt.exe' in downloaded archive"
            }
        }

        Copy-Item -Path $beltExe -Destination (Join-Path $installDir "belt.exe") -Force
    }
    finally {
        # Cleanup temp directory
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }

    Write-Installer "belt $version installed to $(Join-Path $installDir 'belt.exe')"

    # Update PATH
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($currentPath -and $currentPath.Split(";") -contains $installDir) {
        Write-Installer "belt is already in your PATH"
    }
    else {
        try {
            if ($currentPath) {
                $newPath = "$installDir;$currentPath"
            }
            else {
                $newPath = $installDir
            }
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            Write-Installer "added $installDir to User PATH"
        }
        catch {
            Write-Host ""
            Write-Installer "could not update PATH automatically. Please add the following to your PATH manually:"
            Write-Host ""
            Write-Host "    $installDir"
            Write-Host ""
        }

        # Update current session PATH
        $env:Path = "$installDir;$env:Path"
    }

    Write-Host ""
    Write-Installer "run 'belt --version' to verify the installation"
}

Install-Belt
