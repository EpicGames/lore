Set-Location J:\test\lore-new-api

$grpc = "localhost:41337"
$http = "http://localhost:41339"
$protoRoot = ".\lore-proto\proto"

# Run this and copy the Token from the block WITHOUT a Resource field.
lore auth list --with-token

$env:LORE_GRPC_TOKEN = "eyJhbGciOiJSUzI1NiIsImtpZCI6IlZ4N1hNOTI5eTd3QUpwUmNzeDdCb3RVTElhSTZOWnVuIn0.eyJzdWIiOiIyZmExMDFjOC1hZDRlLTRiYWEtYjdhNC0wZjk3NmY0YjRhMDAiLCJpYXQiOjE3ODQyNTUwMjMsImV4cCI6MTc4Njg0NzAyMywibmFtZSI6IllhaXIiLCJ1c2VyX2lkIjoiMmZhMTAxYzgtYWQ0ZS00YmFhLWI3YTQtMGY5NzZmNGI0YTAwIiwicHJlZmVycmVkX3VzZXJuYW1lIjoib3p5YWlyODVAZ21haWwuY29tIiwiaXNfc2VydmljZV9hY2NvdW50IjpmYWxzZSwiZW52IjoiREVGQVVMVCIsImlkcCI6ImJldHRlci1hdXRoIiwiaXNzIjoibG9yZS1hdXRoLmxvY2FsIiwiYXVkIjpbImxvcmUueW91cmRvbWFpbi5jb20iLCJsb2NhbGhvc3QiLCIxMjcuMC4wLjEiLCJodHRwOi8vMTI3LjAuMC4xOjg3ODcvIl19.GYyxYvL53GYB0T8kGCtbt-2tGJrlxjxiAHZBXXEfXMC1UYaPDVawp1O9xpLkPaKV4VwrjU6Kox9FrkddlDJ_tgi7JPCfO2DTscxly2kAUdmPLLIaSmKysG-5kkiO6HN55iO0ArQDK2sEmnEHmHtPvH9eDlL_Wd1gF-kIDjzdviAn3YMTZsfVt_ZO5Fpwgdn_NmpvK39QrDfYGMrGbgOH1r_dlQVGyJotd-5bdizSGwZ19BU-46eTfE7DsFzYgW1UcU8_jMHUY3Jo76otBn1BlwdYFgiFxN-y3arAfxCB62wFJA5XLplpBDV6j_-2gj3n3xPx5SAuqKY73Hbhmt_tBw"

$baseArgs = @("-plaintext", "-import-path", $protoRoot, "-expand-headers", "-rpc-header", 'authorization: Bearer ${LORE_GRPC_TOKEN}')


# '{}' | & grpcurl @baseArgs `
#     -proto "lore/repository/v1/repository.proto" `
#     -d '@' `
#     $grpc `
#     "lore.repository.v1.RepositoryService/RepositoryList"


$repoName = "space9"

$repoRequest = @{
    name = $repoName
} | ConvertTo-Json -Compress

$repoText = ($repoRequest | & grpcurl @baseArgs `
        -proto "lore/repository/v1/repository.proto" `
        -d '@' `
        $grpc `
        "lore.repository.v1.RepositoryService/RepositoryGet") -join "`n"

$repo = $repoText | ConvertFrom-Json

$repoB64 = $repo.repository.id
$branchB64 = $repo.repository.defaultBranchId

$repoHex = (
    [Convert]::FromBase64String($repoB64) |
    ForEach-Object { $_.ToString("x2") }
) -join ""

"Repository resource: $repoHex"
"Default branch: $($repo.repository.defaultBranchName)"

$env:LORE_GRPC_TOKEN = "eyJhbGciOiJSUzI1NiIsImtpZCI6IlZ4N1hNOTI5eTd3QUpwUmNzeDdCb3RVTElhSTZOWnVuIn0.eyJzdWIiOiIyZmExMDFjOC1hZDRlLTRiYWEtYjdhNC0wZjk3NmY0YjRhMDAiLCJpYXQiOjE3ODQyNTUwMjgsImV4cCI6MTc4Njg0NzAyOCwibmFtZSI6IllhaXIiLCJ1c2VyX2lkIjoiMmZhMTAxYzgtYWQ0ZS00YmFhLWI3YTQtMGY5NzZmNGI0YTAwIiwicHJlZmVycmVkX3VzZXJuYW1lIjoib3p5YWlyODVAZ21haWwuY29tIiwiaXNfc2VydmljZV9hY2NvdW50IjpmYWxzZSwiZW52IjoiREVGQVVMVCIsImlkcCI6ImJldHRlci1hdXRoIiwicmVzb3VyY2VzIjpbeyJyZXNvdXJjZV9pZCI6InVyYy02OTRiMTA4ZmEyMTA0YzNlYjkyNDZmNTU5YTRlYTk4OCIsInBlcm1pc3Npb24iOlsicmVhZCIsIndyaXRlIiwiYWRtaW4iLCJvd25lciIsIm9ibGl0ZXJhdGUiLCJtaWdyYXRlIl19XSwiZ3JvdXBzIjpbXSwiaXNzIjoibG9yZS1hdXRoLmxvY2FsIiwiYXVkIjpbImxvcmUueW91cmRvbWFpbi5jb20iLCJsb2NhbGhvc3QiLCIxMjcuMC4wLjEiLCJodHRwOi8vMTI3LjAuMC4xOjg3ODcvIl19.o3Xd9qDqgzB_Q8VsTO8oPSS8S15bB-pBpRV5cAuycLeSXA4HbihR1MPnbyE5VORuO5XnrBQCAor3MX8Ht_qW3W9kUf_C-ARkp9HiKmsIq7PNGWR3ttXhUMHPAgmJEvLEvn_Dg25Ky5mVsmsDBldVUotpGM1gWzAPhxxAM0_Hyke0H1lc9E3jb1bevxN6JWktH__Kbu2SInbDhJZBjkDoe_SDGzpGRquRimyjTMTDmtjYixfKDBlYhTZUAN2I5EG8BhYeBZkkgCKRefMmgUqPGKAsXjS_9TrvsBNS7q28OprhQ3GET5DiDBw21wAjTnEkswrwcpq9PUW0-Mqhwwzshw"

$repoArgs = $baseArgs + @(
    "-rpc-header", "urc-repository-id-bin: $repoB64"
)

$treeRequest = @{
    identifier = @{
        branchId = $branchB64
        number   = "0"
    }
    maxDepth   = 3
} | ConvertTo-Json -Depth 5 -Compress

$treeRequest | & grpcurl @repoArgs `
    -proto "lore/thin_client/v1/thin_client.proto" `
    -d '@' `
    $grpc `
    "lore.thin_client.v1.ThinClientService/RevisionTree"


$filePath = "tes.txt"

$downloadRequest = @{
    identifier  = @{
        branchId = $branchB64
        number   = "0"
    }
    path        = $filePath
    ttlSeconds  = "300"
    contentType = "application/octet-stream"
    inline      = $false
} | ConvertTo-Json -Depth 5 -Compress

$downloadText = ($downloadRequest | & grpcurl @repoArgs `
        -proto "lore/thin_client/v1/thin_client.proto" `
        -d '@' `
        $grpc `
        "lore.thin_client.v1.ThinClientService/RevisionFileDownload") -join "`n"

$download = $downloadText | ConvertFrom-Json
$download | Format-List

$url = $http + $download.urlSuffix
$url