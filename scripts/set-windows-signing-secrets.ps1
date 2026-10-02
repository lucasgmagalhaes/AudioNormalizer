param(
  [Parameter(Mandatory)]
  [ValidateScript({ Test-Path $_ -PathType Leaf })]
  [string]$CertificatePath,

  [Parameter(Mandatory)]
  [string]$Repository
)

$password = Read-Host "PFX password" -AsSecureString
$credential = [System.Net.NetworkCredential]::new("", $password)
$plainPassword = $credential.Password
$certificate = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Resolve-Path $CertificatePath)))

try {
  $certificate | gh secret set WINDOWS_CERTIFICATE_BASE64 --repo $Repository
  $plainPassword | gh secret set WINDOWS_CERTIFICATE_PASSWORD --repo $Repository
  if ($LASTEXITCODE -ne 0) { throw "GitHub CLI could not save the signing secrets." }
  Write-Host "Windows signing secrets configured for $Repository."
}
finally {
  $plainPassword = $null
  $certificate = $null
}
