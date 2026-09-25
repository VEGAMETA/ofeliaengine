# Posts Markdown files as one comment on the commit under test.
#
# Job logs and artifacts live in Azure blob storage, which automation working on this
# repository cannot reach; commit comments are readable through the GitHub REST API and
# show up on the commit and in its pull request.
#
# Environment: GH_TOKEN (contents: write), REPORT_SHA, RUN_URL, GITHUB_API_URL,
# GITHUB_REPOSITORY.
param(
    [Parameter(Mandatory = $true)][string]$Title,
    [Parameter(Mandatory = $true)][string[]]$Files
)
$ErrorActionPreference = 'Continue'
# `powershell -File` passes "a.md,b.md" as one string; accept both forms.
$Files = $Files | ForEach-Object { $_ -split ',' } | Where-Object { $_ }

$md = @("### $Title", "", "Run: $env:RUN_URL", "")
foreach ($file in $Files) {
    if (Test-Path $file) {
        $md += Get-Content $file
    } else {
        $md += "_Missing: ${file}_"
    }
    $md += ""
}
$body = ($md -join "`n")
if ($body.Length -gt 60000) { $body = $body.Substring(0, 60000) + "`n... (truncated)" }

$json = @{ body = $body } | ConvertTo-Json -Compress
$uri = "$env:GITHUB_API_URL/repos/$env:GITHUB_REPOSITORY/commits/$env:REPORT_SHA/comments"
try {
    Invoke-RestMethod -Method Post -Uri $uri -Body ([System.Text.Encoding]::UTF8.GetBytes($json)) `
        -ContentType 'application/json; charset=utf-8' `
        -Headers @{ Authorization = "Bearer $env:GH_TOKEN"; Accept = 'application/vnd.github+json' } | Out-Null
} catch {
    Write-Error "Failed to post the commit comment: $_"
    exit 1
}
exit 0
