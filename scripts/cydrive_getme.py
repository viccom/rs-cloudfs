"""Resolve the configured bot's identity via the Telegram Bot API getMe.

Reads the bot token from the Windows credential store entry written by
`cydrive setup`/`cydrive migrate` (keyring service "cydrive", key
"bot_token" -> target name "bot_token.cydrive"), then calls getMe through
the local proxy (Clash mixed port). The token is never printed.

Usage:  python scripts/cydrive_getme.py
"""

import ctypes
import ctypes.wintypes as wt
import json
import urllib.request


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


def read_credential(target: str) -> str:
    advapi32 = ctypes.windll.advapi32
    ptr = ctypes.POINTER(CREDENTIAL)()
    if not advapi32.CredReadW(target, 1, 0, ctypes.byref(ptr)):
        raise OSError(f"CredReadW failed: {target}")
    try:
        cred = ptr.contents
        blob = ctypes.string_at(cred.CredentialBlob, cred.CredentialBlobSize)
    finally:
        advapi32.CredFree(ptr)
    # keyring's windows-native store writes the secret as UTF-16 bytes.
    return blob.decode("utf-16-le").rstrip("\x00")


def main() -> None:
    token = read_credential("bot_token.cydrive")
    if ":" not in token:
        raise SystemExit("credential does not look like a bot token")

    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({"https": "http://127.0.0.1:7897"})
    )
    with opener.open(f"https://api.telegram.org/bot{token}/getMe", timeout=25) as resp:
        info = json.load(resp)["result"]

    print(f"bot id={info['id']}  name={info['first_name']}  username=@{info['username']}")


if __name__ == "__main__":
    main()
