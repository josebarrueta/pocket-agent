# Pocket Agent

Pocket Agent coordinates isolated coding jobs requested through interchangeable local or remote ingress adapters.

## Language

**Harness**:
The transport-neutral coordinator that owns job selection, lifecycle, approvals, and routing of results.
_Avoid_: Controller, Signal bot

**Ingress**:
A source of authenticated requests to the harness, such as the local CLI or Signal.
_Avoid_: Messenger, entry point

**Principal**:
The authenticated person or system allowed to submit requests through an ingress.
_Avoid_: Sender, phone number, user

**Conversation**:
A routing scope that owns an active-job selection and approval requests. It is not necessarily a Signal conversation.
_Avoid_: Chat, thread

**Job**:
An isolated, resumable coding session bound to one principal, conversation, and configured repository alias.
_Avoid_: Agent, container, run

**Turn**:
One prompt and its result within a job. A successful turn leaves its job available for another turn.
_Avoid_: Job, request

**Worker**:
The disposable untrusted sandbox process in which Pi and agent-selected tools execute.
_Avoid_: Harness, agent

**Reply**:
A structured harness event delivered to the ingress that originated a request.
_Avoid_: Signal message, console line
