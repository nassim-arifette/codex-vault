param([string] $OutputDirectory = (Join-Path $PSScriptRoot '../validation/synthetic'))
$ErrorActionPreference = 'Stop'
$corpusRoot = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $corpusRoot | Out-Null
$fixtureCases = @()
function New-Record($Type, $Payload) {
    @{timestamp='2026-01-01T00:00:00.000Z'; type=$Type; payload=$Payload}
}
function New-Turn($Id, $Project, $Text) {
    New-Record 'event_msg' @{type='task_started'; turn_id=$Id; model_context_window=200000}
    New-Record 'turn_context' @{turn_id=$Id; cwd=$Project; approval_policy='never'; sandbox_policy=@{type='read-only'}; model='gpt-5.4'; effort='medium'; summary='auto'}
    New-Record 'event_msg' @{type='user_message'; message=$Text; images=@(); local_images=@(); text_elements=@()}
    New-Record 'response_item' @{type='message'; role='user'; content=@(@{type='input_text'; text=$Text})}
    New-Record 'response_item' @{type='message'; role='assistant'; content=@(@{type='output_text'; text='Synthetic answer.'})}
    New-Record 'event_msg' @{type='task_complete'; turn_id=$Id; last_agent_message='Synthetic answer.'}
}
foreach ($caseNumber in 1..4) {
    $sessionId = ('11111111-1111-4111-8111-{0:D12}' -f $caseNumber)
    $project = Join-Path $corpusRoot ('project-' + $(if ($caseNumber -eq 4) {'beta'} else {'alpha'}))
    New-Item -ItemType Directory -Force -Path $project | Out-Null
    $records = @(New-Record 'session_meta' @{id=$sessionId; timestamp='2026-01-01T00:00:00.000Z'; cwd=$project; originator='codex_cli_rs'; cli_version='0.152.1'; source='cli'; model_provider='mock'; base_instructions=@{text='Synthetic compatibility test. Answer briefly.'}})
    $records += @(New-Turn 'old-turn' $project 'Historical synthetic authentication decision: use rotating refresh tokens.')
    $repeatCount = if ($caseNumber -eq 2) {20000} else {100}
    $records += New-Record 'response_item' @{type='message'; role='assistant'; content=@(@{type='output_text'; text=('Synthetic historical detail. ' * $repeatCount)})}
    if ($caseNumber -ne 4) {
        $records += New-Record 'compacted' @{message='Synthetic checkpoint'; replacement_history=@(@{type='message'; role='user'; content=@(@{type='input_text'; text='Synthetic checkpoint summary: authentication uses rotating refresh tokens.'})}); window_number=3}
    }
    $records += @(New-Turn 'recent-turn' $project 'Continue the synthetic project.')
    if ($caseNumber -eq 3) {
        $records += New-Record 'compacted' @{message='Synthetic second checkpoint'; replacement_history=@(@{type='message'; role='user'; content=@(@{type='input_text'; text='Synthetic second checkpoint summary.'})}); window_number=4}
        $records += @(New-Turn 'latest-turn' $project 'Finish the synthetic task.')
    }
    $name = 'rollout-2026-01-01T00-00-00-' + $sessionId + '.jsonl'
    $path = Join-Path $corpusRoot $name
    $lines = @($records | ForEach-Object { ConvertTo-Json -InputObject $_ -Depth 20 -Compress })
    [IO.File]::WriteAllText($path, ($lines -join "`n") + "`n", [Text.UTF8Encoding]::new($false))
    $fixtureCases += @{name=('synthetic-case-' + $caseNumber); session_id=$sessionId; path=$path}
}
$casePath = Join-Path $corpusRoot 'cases.json'
[IO.File]::WriteAllText($casePath, (ConvertTo-Json -InputObject $fixtureCases -Depth 5), [Text.UTF8Encoding]::new($false))
Write-Output $casePath
