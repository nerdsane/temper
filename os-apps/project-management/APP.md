# Project Management

Linear-style project management with issues, projects, cycles, labels, and comments. Planning and execution are separated -- one agent drafts the plan, a supervisor approves it, and a (potentially different) agent implements it. Cedar policies enforce role separation.

## Entity Types

### Issue

Core work item. Models the full lifecycle from backlog triage through planning, execution, review, and archival.

**States**: Backlog → Triage → Todo → Planning → Planned → InProgress → InReview → Done → Cancelled → Archived

**Key actions**:
- **SetPriority**: Set priority level (1=urgent, 2=high, 3=medium, 4=low). Required before Todo.
- **AssignPlanner**: Assign a planner agent to draft the implementation plan
- **BeginPlanning**: Start planning phase (requires planner)
- **WritePlan**: Draft the implementation plan with acceptance criteria
- **ApprovePlan / RejectPlan**: Supervisor gates the plan
- **Assign**: Assign an implementer (can differ from planner)
- **StartWork**: Begin implementation (requires assignee and approved plan)
- **UpdateImplementation**: Drop progress notes, blockers, findings
- **SubmitForReview**: Submit work for code review
- **RequestChanges / ApproveReview**: Review cycle
- **Reassign**: Reassign to a different implementer from any active state
- **AddComment / AddLabel / SetParent / AddSubIssue**: Metadata and hierarchy
- **Cancel / Archive**: Termination paths

### Project

Groups issues into a logical project with lifecycle tracking.

**States**: Planning → Active → Paused → Completed → Archived

**Key actions**:
- **SetDescription**: Set project description (required before activation)
- **Activate**: Start the project
- **AddIssue / AddCycle / AddMember**: Populate the project
- **Pause / Resume**: Temporarily halt work
- **Complete / Archive**: Terminal states

### Cycle

Time-boxed sprint within a project.

**States**: Planning → Active → Completed

**Key actions**:
- **SetProject**: Associate with a project
- **AddIssueToCycle / RemoveIssueFromCycle**: Manage sprint scope
- **Start**: Begin the sprint (requires at least one issue)
- **MarkIssueComplete**: Record issue completion within the cycle
- **Complete**: End the sprint

### Comment

Discussion comment on an issue, traceable to the agent session that created it.

**States**: Active → Edited → Deleted

**Key actions**:
- **Create**: Initialize with body, author, agent type, and session ID
- **Edit**: Update the comment body
- **React**: Add a reaction
- **Delete**: Soft-delete (terminal)

### Label

Categorization tag for issues.

**States**: Active → Archived

**Key actions**:
- **Create**: Initialize with name, color, and description
- **IncrementUsage**: Track attachment count
- **Archive**: Terminal state

## Setup

```
temper.install_app("<tenant>", "project-management")
```
