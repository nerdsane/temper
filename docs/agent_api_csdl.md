# Agent API: CSDL-Based (`/tdata`)

## Direction

In this service, agent APIs are CSDL-based and served via `/tdata`.
There is no separate `/v1/agents` control-plane API for this path.

## Model

Define the control-plane resources as standard CSDL entities backed by IOA:
- `AgentDefinition`
- `ProgramDefinition`
- `Process`

Properties:
- `id` (key)
- `name`
- `system_prompt`
- `model_provider`
- `model_name`
- `model_max_tokens` (optional)
- `tools_json` (JSON-encoded string array)
- `labels_json` (JSON-encoded string map)
- `created_at`
- `updated_at`

`Process` (API-only phase) fields:
- `id` (key)
- `definition_kind` (`agent` or `program`)
- `definition_id`
- `status`
- `user_prompt`
- `last_input_json`
- `error`
- `created_at`
- `updated_at`
- `started_at`
- `terminated_at`

## CSDL

```xml
<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AgentV1" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="AgentDefinition">...</EntityType>
      <EntityType Name="ProgramDefinition">...</EntityType>
      <EntityType Name="Process">...</EntityType>

      <Action Name="StartProcess" IsBound="true">...</Action>
      <Action Name="SendInput" IsBound="true">...</Action>
      <Action Name="SuspendProcess" IsBound="true">...</Action>
      <Action Name="ResumeProcess" IsBound="true">...</Action>
      <Action Name="TerminateProcess" IsBound="true">...</Action>

      <EntityContainer Name="Container">
        <EntitySet Name="AgentDefinitions" EntityType="Temper.AgentV1.AgentDefinition"/>
        <EntitySet Name="ProgramDefinitions" EntityType="Temper.AgentV1.ProgramDefinition"/>
        <EntitySet Name="Processes" EntityType="Temper.AgentV1.Process"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
```

Canonical concrete fixture:
- `test-fixtures/specs/agent_definition.csdl.xml`

## IOA

```toml
[automaton]
name = "AgentDefinition"
states = ["Ready"]
initial = "Ready"

[[action]]
name = "Touch"
kind = "input"
from = ["Ready"]
to = "Ready"
```

## API Surface

Create:
```http
POST /tdata/AgentDefinitions
```

Read:
- `GET /tdata/AgentDefinitions('ci-fixer-001')`
- `GET /tdata/AgentDefinitions`

Update:
- `PATCH /tdata/AgentDefinitions('ci-fixer-001')`

Delete:
- `DELETE /tdata/AgentDefinitions('ci-fixer-001')`

Process CRUD:
- `POST /tdata/Processes`
- `GET /tdata/Processes('proc-001')`
- `GET /tdata/Processes`
- `PATCH /tdata/Processes('proc-001')`
- `DELETE /tdata/Processes('proc-001')`

Process bound actions (phase 1 = API-only state transitions):
- `POST /tdata/Processes('proc-001')/Temper.AgentV1.StartProcess`
- `POST /tdata/Processes('proc-001')/Temper.AgentV1.SendInput`
- `POST /tdata/Processes('proc-001')/Temper.AgentV1.SuspendProcess`
- `POST /tdata/Processes('proc-001')/Temper.AgentV1.ResumeProcess`
- `POST /tdata/Processes('proc-001')/Temper.AgentV1.TerminateProcess`

Scope note for this phase:
- These actions only update entity state/fields via IOA transitions.
- No execution loop, tool dispatch, or scheduler integration yet.

## Example Payload

```json
{
  "id": "ci-fixer-001",
  "name": "ci-fixer",
  "system_prompt": "You diagnose CI failures and propose fixes.",
  "model_provider": "anthropic",
  "model_name": "claude-sonnet-4-6",
  "model_max_tokens": 8192,
  "tools_json": "[\"datadog_logs_search\",\"bash\"]",
  "labels_json": "{\"team\":\"ci-platform\"}",
  "created_at": "2026-03-11T10:00:00Z",
  "updated_at": "2026-03-11T10:00:00Z"
}
```
