# Resolve the configured bot's identity via the Telegram Bot API getMe.
# PowerShell variant of cydrive_getme.py (keep whichever you prefer).
# Reads the token from the credential store entry "bot_token.cydrive"
# (written by cydrive setup/migrate; keyring stores the blob as UTF-16),
# then calls getMe through the local proxy. The token is never printed.
#
# Usage:  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\cydrive_getme.ps1

$sig = @'
using System;
using System.Runtime.InteropServices;
public class CredMan3 {
    [DllImport("advapi32.dll", EntryPoint="CredReadW", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool CredRead(string target, int type, int flags, out IntPtr credPtr);
    [DllImport("advapi32.dll")]
    public static extern void CredFree(IntPtr cred);
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    public struct CREDENTIAL {
        public int Flags; public int Type; public string TargetName; public string Comment;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWritten;
        public int CredentialBlobSize; public IntPtr CredentialBlob; public int Persist;
        public int AttributeCount; public IntPtr Attributes; public string TargetAlias; public string UserName;
    }
    public static byte[] ReadBytes(string target) {
        IntPtr ptr;
        if (!CredRead(target, 1, 0, out ptr)) return null;
        try {
            CREDENTIAL cred = (CREDENTIAL)Marshal.PtrToStructure(ptr, typeof(CREDENTIAL));
            byte[] blob = new byte[cred.CredentialBlobSize];
            Marshal.Copy(cred.CredentialBlob, blob, 0, cred.CredentialBlobSize);
            return blob;
        } finally { CredFree(ptr); }
    }
}
'@
Add-Type -TypeDefinition $sig
$blob = [CredMan3]::ReadBytes('bot_token.cydrive')
$token = [Text.Encoding]::Unicode.GetString($blob).Trim("`0")
if ($token -notmatch '^\d+:[A-Za-z0-9_-]{20,}$') { Write-Output 'BAD_TOKEN'; exit 1 }
try {
    $req = [Net.HttpWebRequest]::Create("https://api.telegram.org/bot$token/getMe")
    $req.Proxy = New-Object System.Net.WebProxy('http://127.0.0.1:7897')
    $req.Timeout = 25000
    try { $resp = $req.GetResponse() } catch [Net.WebException] { $resp = $_.Exception.Response }
    if (-not $resp) { Write-Output "transport_error: $($_.Exception.Message)"; exit 1 }
    $reader = New-Object IO.StreamReader($resp.GetResponseStream())
    $body = ($reader.ReadToEnd()) -replace [regex]::Escape($token), '<TOKEN>'
    Write-Output $body
} catch { Write-Output ("unexpected: " + $_.Exception.Message) }
