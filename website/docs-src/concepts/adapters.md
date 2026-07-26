---
title: Adapters
description: Supervised connections that receive external events and wake conversations.
---

# Adapters

Adapters are long-running, host-managed bridges to external systems such as
ExoChat, IRC, WhatsApp, Signal, Discord, and local clients.

They are separate from [tools](./tools):

- A tool runs for a bounded model call during a turn.
- An adapter remains available between turns.
- An inbound adapter event can wake a conversation.
- Outbound effects require an explicit tool call; final model text is never
  sent implicitly.

## Workers and durable state

Adapters run as supervised worker processes. The host owns lifecycle, routing,
durable inbox/outbox records, retries, and conversation wakeups. Each worker
owns its protocol-specific connection, event normalization, and
acknowledgements.

Adapter identity, provider cursors, deduplication keys, routing, pending sends,
and health survive worker restarts. Queuing an outbound message is not the same
as provider-confirmed delivery.

Credentials are secret references in adapter configuration. The host resolves
their values when starting an authorized worker; raw credentials do not belong
in prompts or durable configuration.

## Current management model

The current adapter lifecycle and messaging tools remain the practical control
surface. Installations can select adapters such as ExoChat or protocol-specific
workers and inspect their health through the existing runtime.

A universal adapter command envelope and manifest-generated model tools are
deferred. Protocol-specific commands may remain explicit tools until there is
enough experience to justify a generic command system. Remote adapter
registries and signed distribution are also outside the current plan.

## Scheduler

The scheduler remains the current scheduler service with its existing tools.
It stores schedules and wakes conversations when work is due, but it is not
modeled or documented as an adapter in this phase.

Moving scheduling onto the adapter runtime can be reconsidered later. Practical
profiles should include the current scheduler service directly rather than
depending on that future design.

## Profiles

- **Bootstrap** installs no adapters.
- **Practical** starts the current scheduler service and the installation's
  selected adapters.

Profiles are curated defaults. They do not imply remote discovery, signatures,
or a capability sandbox for worker code.

For setup details of currently shipped adapters, see their READMEs under
[`examples/exo/adapters/`](https://github.com/exoharness/exo/tree/main/examples/exo/adapters).
