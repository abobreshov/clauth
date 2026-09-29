#!/usr/bin/env bash
set -euo pipefail

REPO="abobreshov/clauth"
BINARY="tollgate"
NOCARGO=0

for arg in "$@"; do
    case "${arg}" in
        --nocargo) NOCARGO=1 ;;
        *) echo "Unknown argument: ${arg}" >&2; exit 1 ;;
    esac
done

# If cargo is available, prefer it (unless --nocargo was passed)
if [[ "${NOCARGO}" -eq 0 ]] && command -v cargo &>/dev/null; then
    echo "cargo detected, installing via cargo..."
    # From the fork's own repo, never crates.io: a `tollgate` crate there is
    # not this tool.
    cargo install --locked --git "https://github.com/${REPO}" tollgate
    echo ""
    # Best-effort: converge plugin installPaths a dead session tree left behind.
    tollgate self-heal || echo "note: plugin path heal skipped" >&2
    echo ""
    echo "To uninstall, run: cargo uninstall tollgate"
    exit 0
fi

# Detect OS and arch
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "${OS}" in
    linux)
        case "${ARCH}" in
            x86_64) ASSET="tollgate-linux-x86_64" ;;
            *) echo "Unsupported architecture: ${ARCH}" >&2; exit 1 ;;
        esac
        ;;
    darwin)
        case "${ARCH}" in
            x86_64)        ASSET="tollgate-macos-x86_64" ;;
            arm64|aarch64) ASSET="tollgate-macos-aarch64" ;;
            *)             echo "Unsupported architecture: ${ARCH}" >&2; exit 1 ;;
        esac
        ;;
    *mingw*|*msys*|*cygwin*)
        ASSET="tollgate-windows-x86_64.exe"
        BINARY="tollgate.exe"
        OS="windows"
        ;;
    *)
        echo "Unsupported OS: ${OS}" >&2
        echo "Install via cargo: cargo install --locked --git https://github.com/${REPO} tollgate"
        exit 1
        ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
echo "Downloading ${ASSET}..."

TMP=$(mktemp)
TMP_SUMS=$(mktemp)
trap 'rm -f "${TMP}" "${TMP_SUMS}"' EXIT

if command -v curl &>/dev/null; then
    curl -fsSL "${URL}" -o "${TMP}"
elif command -v wget &>/dev/null; then
    wget -q "${URL}" -O "${TMP}"
else
    echo "Error: curl or wget is required" >&2
    exit 1
fi

# Verify integrity against sha256sums.txt from the same release
SUMS_URL="https://github.com/${REPO}/releases/latest/download/sha256sums.txt"
echo "Verifying checksum..."

if command -v curl &>/dev/null; then
    curl -fsSL "${SUMS_URL}" -o "${TMP_SUMS}" \
        || { echo "Error: failed to download sha256sums.txt. Aborting install" >&2; exit 1; }
elif command -v wget &>/dev/null; then
    wget -q "${SUMS_URL}" -O "${TMP_SUMS}" \
        || { echo "Error: failed to download sha256sums.txt. Aborting install" >&2; exit 1; }
fi

# Detect portable sha256 tool
if command -v sha256sum &>/dev/null; then
    ACTUAL_HEX=$(sha256sum "${TMP}" | awk '{print $1}')
elif command -v shasum &>/dev/null; then
    ACTUAL_HEX=$(shasum -a 256 "${TMP}" | awk '{print $1}')
else
    echo "Error: sha256sum or shasum is required for integrity verification" >&2
    exit 1
fi

# Extract expected hex from sums file: lines are "<64-hex>  <asset-name>"
EXPECTED_HEX=$(grep -E "^[0-9a-f]{64}  ${ASSET}$" "${TMP_SUMS}" | awk '{print $1}')

if [[ -z "${EXPECTED_HEX}" ]]; then
    echo "Error: ${ASSET} not found in sha256sums.txt. Aborting install" >&2
    exit 1
fi

if [[ "${ACTUAL_HEX}" != "${EXPECTED_HEX}" ]]; then
    echo "Error: checksum mismatch for ${ASSET}" >&2
    printf '  expected: %s\n' "${EXPECTED_HEX}" >&2
    printf '  got:      %s\n' "${ACTUAL_HEX}" >&2
    exit 1
fi

echo "Checksum verified."
chmod +x "${TMP}"

# Choose install directory
if [[ "${OS}" == "windows" ]]; then
    INSTALL_DIR="${HOME}/.local/bin"
elif [[ -w /usr/local/bin ]]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="${HOME}/.local/bin"
fi

mkdir -p "${INSTALL_DIR}"
mv "${TMP}" "${INSTALL_DIR}/${BINARY}"

echo "Installed to ${INSTALL_DIR}/${BINARY}"

# Best-effort: converge plugin installPaths a dead session tree left behind.
"${INSTALL_DIR}/${BINARY}" self-heal || echo "note: plugin path heal skipped" >&2

# Warn if install dir is not in PATH
if ! printf '%s' "${PATH}" | grep -q "${INSTALL_DIR}"; then
    echo ""
    echo "Note: ${INSTALL_DIR} is not in your PATH. Add this to your shell profile:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
fi

echo ""
echo "To uninstall, run: rm ${INSTALL_DIR}/${BINARY}"
