# 🔭 Vantage: Spec for Streaming Trajectories via SSE/WebSockets

## 👤 User Story
"As a Developer running the agent locally, I want to stream agent steps and outputs in real-time, so that I can monitor progress without waiting for the full trajectory file to be written at the end of the run."

## ✅ Acceptance Criteria
- Must implement real-time streaming of trajectory steps (using a mechanism like SSE or WebSockets).
- Must emit step outcomes (e.g. commands run, exit codes, bash output) as they occur.
- Must handle network disconnects gracefully without crashing the underlying agent process.

## 🚫 Out of Scope
- Full UI web application (Phase 2).
- History playback of old runs over streaming endpoints (Phase 2).
