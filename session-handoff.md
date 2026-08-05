# Local Agent Session Handoff

## Current Objective

Local LAN share guests can watch the host session, run shell commands, start Agent Mode with `/agent`, and continue the open Agent View conversation with plain follow-ups.

## Last Updated

2026-08-04

## Active Feature

(none)

## Branch

- `cla-dev-2`

## Current State

- Guests join as **Executor** with a dedicated `#guestbar` command line.
- Guest line routing (`Input::submit_line_on_behalf_of_shared_session_participant`):
  1. Slash / skill → AI slash stack (`/agent` always starts a **new** conversation).
  2. Else if Agent View is active → `submit_user_query_now` (follow-up on the selected conversation).
  3. Else → shell via `try_execute_command_on_behalf_of_shared_session_participant`.
- Agent Mode turns are mirrored as `LocalShareAgentExchange` with markdown rendering and publish-order replay on join/rotate.
- WarpOss must be rebuilt with `./script/bundle` after these changes (`cargo build --bin warp` alone does not update the app bundle).

## Recommended Next Step

Rebuild and relaunch WarpOss, then dogfood guest follow-ups: `/agent …` once, then a plain-text follow-up without `/agent`.
