"""Resolve the configured bot's identity via the Telegram Bot API getMe.

Cross-platform (Windows / Linux / macOS). The bot token is discovered
with the same priority chain as cydrive itself:

  1. CYDRIVE_BOT_TOKEN env
  2. ./config.toml  (bot_token = "..." — the headless-setup form)
  3. ./config.json  (legacy Python config)
  4. the OS credential store the keyring crate uses:
       Windows  Credential Manager, target "bot_token.cydrive" (UTF-16 blob)
       macOS    Keychain,            `security find-generic-password -s cydrive -a bot_token -w`
       Linux    Secret Service,      `secret-tool lookup service cydrive username bot_token`

Proxy resolution: --proxy flag > CYDRIVE_GETME_PROXY > https_proxy/http_proxy
env > config.toml proxy_url (socks5:// rewritten to http:// — Clash-style
mixed ports speak both) > http://127.0.0.1:7897. Without --proxy a direct
attempt runs first, the proxy is the fallback.

The token itself is NEVER printed; only its source is.

Usage:  python cydrive_getme.py [--proxy URL] [--timeout SECS]
"""

import argparse
import ctypes
import ctypes.wintypes as wt
import json
import os
import re
import subprocess
import sys
import urllib.request

SERVICE = "cydrive"
KEY = "bot_token"
DEFAULT_PROXY = "http://127.0.0.1:7897"


# --------------------------------------------------------------------------
# token discovery


def token_from_env():
    value = os.environ.get("CYDRIVE_BOT_TOKEN", "").strip()
    return value or None


def _kv_from_toml(text, key):
    for line in text.splitlines():
        match = re.match(rf'\s*{re.escape(key)}\s*=\s*"([^"]*)"', line)
        if match and match.group(1):
            return match.group(1)
    return None


def token_from_config():
    """config.toml first, then the legacy config.json — cwd, like cydrive."""
    try:
        with open("config.toml", encoding="utf-8") as fh:
            return _kv_from_toml(fh.read(), "bot_token")
    except OSError:
        pass
    try:
        with open("config.json", encoding="utf-8") as fh:
            value = json.load(fh).get("bot_token", "")
            return value if value else None
    except (OSError, ValueError):
        return None


def proxy_from_config():
    try:
        with open("config.toml", encoding="utf-8") as fh:
            raw = _kv_from_toml(fh.read(), "proxy_url")
    except OSError:
        return None
    if not raw:
        return None
    # A Clash-style mixed port serves HTTP CONNECT on the same address;
    # urllib speaks http proxies only, so socks5:// is rewritten.
    return re.sub(r"^socks5h?://", "http://", raw)


def token_from_windows_credman():
    class CREDENTIAL(ctypes.Structure):
        _fields_ = [
            ("Flags", wt.DWORD), ("Type", wt.DWORD), ("TargetName", wt.LPWSTR),
            ("Comment", wt.LPWSTR), ("LastWritten", wt.FILETIME),
            ("CredentialBlobSize", wt.DWORD),
            ("CredentialBlob", ctypes.POINTER(ctypes.c_byte)),
            ("Persist", wt.DWORD), ("AttributeCount", wt.DWORD),
            ("Attributes", ctypes.c_void_p),
            ("TargetAlias", wt.LPWSTR), ("UserName", wt.LPWSTR),
        ]

    advapi32 = ctypes.windll.advapi32
    ptr = ctypes.POINTER(CREDENTIAL)()
    if not advapi32.CredReadW(f"{KEY}.{SERVICE}", 1, 0, ctypes.byref(ptr)):
        return None
    try:
        cred = ptr.contents
        blob = ctypes.string_at(cred.CredentialBlob, cred.CredentialBlobSize)
    finally:
        advapi32.CredFree(ptr)
    # keyring's windows-native store writes the secret as UTF-16 bytes.
    return blob.decode("utf-16-le").rstrip("\x00") or None


def _run(argv):
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return out.stdout.strip() if out.returncode == 0 else None


def token_from_macos_keychain():
    return _run(["security", "find-generic-password", "-s", SERVICE, "-a", KEY, "-w"])


def token_from_secret_service():
    return _run(["secret-tool", "lookup", "service", SERVICE, "username", KEY])


def discover_token():
    """Returns (token, source-name); the source is printed, never the token."""
    steps = [("CYDRIVE_BOT_TOKEN env", token_from_env),
             ("./config.toml|config.json", token_from_config)]
    if sys.platform == "win32":
        steps.append(("Windows credential store", token_from_windows_credman))
    elif sys.platform == "darwin":
        steps.append(("macOS Keychain", token_from_macos_keychain))
    else:
        steps.append(("Secret Service (secret-tool)", token_from_secret_service))
    for source, fn in steps:
        try:
            token = fn()
        except Exception as exc:  # a broken backend must not end the hunt
            print(f"  source {source}: unusable ({exc})", file=sys.stderr)
            token = None
        if token and ":" in token:
            return token, source
    return None, None


# --------------------------------------------------------------------------
# getMe


def get_me(token, proxy, timeout):
    openers = []
    if proxy:
        openers.append(urllib.request.ProxyHandler({"https": proxy, "http": proxy}))
    opener = urllib.request.build_opener(*openers)
    with opener.open(f"https://api.telegram.org/bot{token}/getMe", timeout=timeout) as resp:
        return json.load(resp)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--proxy", help="explicit proxy URL; disables the direct attempt")
    ap.add_argument("--timeout", type=float, default=8.0, help="per-attempt timeout (s)")
    args = ap.parse_args()

    token, source = discover_token()
    if not token:
        sys.exit("no bot token found (env CYDRIVE_BOT_TOKEN, config.toml/json, OS keyring)")
    print(f"token source: {source}")

    def attempt(proxy, label):
        try:
            info = get_me(token, proxy, args.timeout)["result"]
        except Exception as exc:
            print(f"  {label}: {type(exc).__name__}: {exc}", file=sys.stderr)
            return None
        print(f"bot id={info['id']}  name={info['first_name']}  username=@{info['username']}")
        return True

    if args.proxy:
        sys.exit(0 if attempt(args.proxy, f"via {args.proxy}") else 1)

    if attempt(None, "direct"):
        return
    proxy = (
        os.environ.get("CYDRIVE_GETME_PROXY")
        or os.environ.get("https_proxy")
        or os.environ.get("http_proxy")
        or proxy_from_config()
        or DEFAULT_PROXY
    )
    sys.exit(0 if attempt(proxy, f"via {proxy}") else 1)


if __name__ == "__main__":
    main()
