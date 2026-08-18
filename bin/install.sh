#!/usr/bin/env bash

set -euo pipefail

# mq-mount installation script

readonly MQM_REPO="harehare/mq-mount"
MQM_BIN_DIR="${MQM_BIN_DIR:-$HOME/.local/bin}"
MQM_MODIFY_PATH=1

# Colors for output
readonly RED='\033[0;31m'
readonly GREEN='\033[0;32m'
readonly YELLOW='\033[1;33m'
readonly BLUE='\033[0;34m'
readonly PURPLE='\033[0;35m'
readonly CYAN='\033[0;36m'
readonly BOLD='\033[1m'
readonly NC='\033[0m' # No Color

# Utility functions
log() {
    echo -e "${GREEN}ℹ${NC}  $*" >&2
}

warn() {
    echo -e "${YELLOW}⚠${NC}  $*" >&2
}

error() {
    echo -e "${RED}✗${NC}  $*" >&2
    exit 1
}

# Display the mq-mount logo
show_logo() {
    cat << 'EOF'

    ███╗   ███╗ ██████╗       ███╗   ███╗ ██████╗ ██╗   ██╗███╗   ██╗████████╗
    ████╗ ████║██╔═══██╗      ████╗ ████║██╔═══██╗██║   ██║████╗  ██║╚══██╔══╝
    ██╔████╔██║██║   ██║█████╗██╔████╔██║██║   ██║██║   ██║██╔██╗ ██║   ██║
    ██║╚██╔╝██║██║▄▄ ██║╚════╝██║╚██╔╝██║██║   ██║██║   ██║██║╚██╗██║   ██║
    ██║ ╚═╝ ██║╚██████╔╝      ██║ ╚═╝ ██║╚██████╔╝╚██████╔╝██║ ╚████║   ██║
    ╚═╝     ╚═╝ ╚══▀▀═╝       ╚═╝     ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═══╝   ╚═╝

EOF
    echo -e "${BOLD}${CYAN}  FUSE-mount a Markdown file as a virtual filesystem${NC}"
    echo -e "${BLUE}  Headings become directories, section bodies become files${NC}"
    echo ""
    echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
}

# Detect the operating system
detect_os() {
    case "$(uname -s)" in
        Linux*)
            echo "linux"
            ;;
        Darwin*)
            echo "darwin"
            ;;
        CYGWIN*|MINGW*|MSYS*)
            echo "windows"
            ;;
        *)
            error "Unsupported operating system: $(uname -s)"
            ;;
    esac
}

# Detect the architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)
            echo "x86_64"
            ;;
        aarch64|arm64)
            echo "aarch64"
            ;;
        *)
            error "Unsupported architecture: $(uname -m)"
            ;;
    esac
}

# Map os/arch to the release target triple
target_triple() {
    local os="$1"
    local arch="$2"

    case "$os" in
        windows)
            if [[ "$arch" != "x86_64" ]]; then
                error "No prebuilt mq-mount binary for windows/$arch"
            fi
            echo "x86_64-pc-windows-msvc"
            ;;
        darwin)
            echo "${arch}-apple-darwin"
            ;;
        linux)
            echo "${arch}-unknown-linux-gnu"
            ;;
    esac
}

# Get the latest release version from GitHub
get_latest_version() {
    local version
    version=$(curl -sf "https://api.github.com/repos/$MQM_REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')

    if [[ -z "$version" ]]; then
        error "Failed to get the latest version"
    fi

    echo "$version"
}

# Download checksums file
download_checksums() {
    local version="$1"
    local checksums_url="https://github.com/$MQM_REPO/releases/download/$version/checksums.txt"
    local checksums_file
    checksums_file=$(mktemp)

    log "Downloading checksums file..."
    if ! curl -Lsf --progress-bar "$checksums_url" -o "$checksums_file"; then
        warn "Failed to download checksums file, skipping verification"
        rm -f "$checksums_file"
        return 1
    fi

    echo "$checksums_file"
}

# Verify binary checksum
verify_checksum() {
    local binary_file="$1"
    local checksums_file="$2"
    local binary_name="$3"

    if [[ ! -f "$checksums_file" ]]; then
        warn "Checksums file not available"
        return 1
    fi

    log "Verifying checksum for $binary_name..."

    local calculated_checksum
    if command -v sha256sum &> /dev/null; then
        calculated_checksum=$(sha256sum "$binary_file" | cut -d' ' -f1)
    elif command -v shasum &> /dev/null; then
        calculated_checksum=$(shasum -a 256 "$binary_file" | cut -d' ' -f1)
    else
        warn "No SHA256 utility found"
        return 1
    fi

    local expected_checksum
    expected_checksum=$(grep "$binary_name/$binary_name" "$checksums_file" | cut -d' ' -f1)

    if [[ -z "$expected_checksum" ]]; then
        warn "No checksum found for $binary_name"
        return 1
    fi

    if [[ "$calculated_checksum" == "$expected_checksum" ]]; then
        log "✓ Checksum verification successful"
        return 0
    else
        echo -e "${RED}✗${NC}  Checksum verification failed" >&2
        echo -e "${RED}Expected: $expected_checksum${NC}" >&2
        echo -e "${RED}Got:      $calculated_checksum${NC}" >&2
        return 1
    fi
}

# Download and install mq-mount
install_mqm() {
    local version="$1"
    local os="$2"
    local target="$3"
    local ext=""
    local binary_name="mq-mount"

    if [[ "$os" == "windows" ]]; then
        ext=".exe"
        binary_name="mq-mount.exe"
    fi

    local release_binary_name="mq-mount-${target}${ext}"
    local download_url="https://github.com/$MQM_REPO/releases/download/$version/$release_binary_name"

    log "Downloading mq-mount $version for $target..."
    log "Download URL: $download_url"

    local checksums_file=""
    checksums_file=$(download_checksums "$version") || true

    mkdir -p "$MQM_BIN_DIR"

    local temp_file
    temp_file=$(mktemp)

    if ! curl -L --progress-bar "$download_url" -o "$temp_file"; then
        rm -f "$temp_file"
        error "Failed to download mq-mount binary"
    fi

    if [[ -n "$checksums_file" && -f "$checksums_file" ]]; then
        if ! verify_checksum "$temp_file" "$checksums_file" "$release_binary_name"; then
            rm -f "$checksums_file" "$temp_file"
            error "Checksum verification failed, aborting installation"
        fi
        rm -f "$checksums_file"
    else
        warn "Checksums file not available, proceeding without verification"
    fi

    mv "$temp_file" "$MQM_BIN_DIR/$binary_name"
    chmod +x "$MQM_BIN_DIR/$binary_name"

    log "mq-mount installed successfully to $MQM_BIN_DIR/$binary_name"
}

# Add mq-mount to PATH by updating shell profile
update_shell_profile() {
    if [[ "$MQM_MODIFY_PATH" -eq 0 ]]; then
        return 0
    fi

    if echo "$PATH" | grep -q "$MQM_BIN_DIR"; then
        log "$MQM_BIN_DIR is already in PATH"
        return 0
    fi

    local shell_profile=""
    local shell_name
    shell_name=$(basename "${SHELL:-}")

    case "$shell_name" in
        bash)
            if [[ -f "$HOME/.bashrc" ]]; then
                shell_profile="$HOME/.bashrc"
            elif [[ -f "$HOME/.bash_profile" ]]; then
                shell_profile="$HOME/.bash_profile"
            fi
            ;;
        zsh)
            if [[ -f "$HOME/.zshrc" ]]; then
                shell_profile="$HOME/.zshrc"
            fi
            ;;
        fish)
            if [[ -d "$HOME/.config/fish" ]]; then
                shell_profile="$HOME/.config/fish/config.fish"
                mkdir -p "$(dirname "$shell_profile")"
            fi
            ;;
    esac

    if [[ -n "$shell_profile" ]]; then
        local path_export
        if [[ "$shell_name" == "fish" ]]; then
            path_export="set -gx PATH \$PATH $MQM_BIN_DIR"
        else
            path_export="export PATH=\"\$PATH:$MQM_BIN_DIR\""
        fi

        if ! grep -q "$MQM_BIN_DIR" "$shell_profile" 2>/dev/null; then
            echo "" >> "$shell_profile"
            echo "# Added by mq-mount installer" >> "$shell_profile"
            echo "$path_export" >> "$shell_profile"
            log "Added $MQM_BIN_DIR to PATH in $shell_profile"
        else
            warn "$MQM_BIN_DIR already exists in $shell_profile"
        fi
    else
        warn "Could not detect shell profile to update"
        warn "Please manually add $MQM_BIN_DIR to your PATH"
    fi
}

# Verify installation
verify_installation() {
    if [[ -x "$MQM_BIN_DIR/mq-mount" ]] || [[ -x "$MQM_BIN_DIR/mq-mount.exe" ]]; then
        log "✓ mq-mount installation verified"
        return 0
    else
        error "mq-mount installation verification failed"
    fi
}

# Show post-installation instructions
show_post_install() {
    local os="$1"

    echo ""
    echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}${GREEN}✨ mq-mount installed successfully! ✨${NC}"
    echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""

    if [[ "$os" == "windows" ]]; then
        echo -e "${BOLD}${YELLOW}⚠ Windows prerequisite:${NC}"
        echo -e "  mq-mount uses ${CYAN}WinFSP${NC} to mount on Windows. Install it first"
        echo -e "  if you haven't already: ${BLUE}https://winfsp.dev${NC}"
        echo ""
    fi

    echo -e "${BOLD}${CYAN}🚀 Getting Started:${NC}"
    echo ""
    echo -e "  ${YELLOW}1.${NC} Restart your terminal or run:"
    echo -e "     ${CYAN}source ~/.bashrc${NC} ${BLUE}(or your shell's profile)${NC}"
    echo ""
    echo -e "  ${YELLOW}2.${NC} Verify the installation:"
    echo -e "     ${CYAN}mq-mount --version${NC}"
    echo ""
    echo -e "  ${YELLOW}3.${NC} Get help:"
    echo -e "     ${CYAN}mq-mount --help${NC}"
    echo ""
    echo -e "${BOLD}${CYAN}⚡ Quick Example:${NC}"
    echo -e "  ${GREEN}▶${NC} ${CYAN}mkdir /tmp/doc-mount${NC}"
    echo -e "  ${GREEN}▶${NC} ${CYAN}mq-mount README.md /tmp/doc-mount${NC}"
    echo -e "  ${GREEN}▶${NC} ${CYAN}ls /tmp/doc-mount${NC}"
    echo ""
    echo -e "${BOLD}${CYAN}📚 Learn More:${NC}"
    echo -e "  ${GREEN}▶${NC} Repository: ${BLUE}https://github.com/$MQM_REPO${NC}"
    echo ""
    echo -e "${PURPLE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo ""
}

# Main installation function
main() {
    show_logo

    if ! command -v curl &> /dev/null; then
        error "curl is required but not installed"
    fi

    local os arch target version
    os=$(detect_os)
    arch=$(detect_arch)
    target=$(target_triple "$os" "$arch")

    log "Detected system: $os/$arch ($target)"

    version=$(get_latest_version)
    log "Latest version: $version"

    install_mqm "$version" "$os" "$target"
    update_shell_profile
    verify_installation
    show_post_install "$os"
}

# Handle script arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --help|-h)
            echo "mq-mount installation script"
            echo ""
            echo "Usage: $0 [options]"
            echo ""
            echo "Options:"
            echo "  --bin-dir <dir>    Install directory (default: \$HOME/.local/bin,"
            echo "                     or \$MQM_BIN_DIR if set)"
            echo "  --no-modify-path   Don't add the install directory to your shell profile"
            echo "  --help, -h         Show this help message"
            echo "  --version, -v      Show installer version and exit"
            exit 0
            ;;
        --version|-v)
            echo "mq-mount installer v1.0.0"
            exit 0
            ;;
        --bin-dir)
            [[ $# -ge 2 ]] || error "--bin-dir requires an argument"
            MQM_BIN_DIR="$2"
            shift
            ;;
        --no-modify-path)
            MQM_MODIFY_PATH=0
            ;;
        *)
            error "Unknown option: $1"
            ;;
    esac
    shift
done

if [[ -z "${BASH_VERSION:-}" ]]; then
    error "This installer requires bash"
fi

main "$@"
