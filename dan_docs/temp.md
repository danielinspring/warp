

C. 빠른 스모크 체크리스트

1. A1 합 계산 — 5050, 가짜 timeout 없음
2. A2 sleep 폴링 — long_running -> poll -> READY
3. A3 python -i write — line write -> 4
4. B1 병렬 2 — run_agents + launched (또는 카드 Accept)
5. B2 5 agents — 거부/재시도 <=4
6. B4 off — run_agents 없음
7. A4/B5 Plan — mutating/run_agents 없음

---

D. 단위 테스트 (이미 통과 기대)

cargo test -p warp local_runtime --lib --features local_ollama_runtime_tool_use

관련 케이스:
- background_shell_tools_map_and_round_trip
- run_agents_is_gated_by_orchestration_and_root_depth
- run_agents_maps_to_local_warp_action_with_child_bounds
- run_agents_result_content_and_proto_cover_launch_and_cancel
- plan mode strip (read_shell_command_output 유지, write/run_agents 제거)

---

E. 모델/환경 주의

- 약한 로컬 모델은 wait_until_complete=false / run_agents를 안 쓸 수 있음 -> 위 프롬프트처럼 도구 이름을 명시하는 편이 재현성 좋음.
- run_agents Accept 후 자식은 Warp 오케스트레이션(로컬 harness) 경로 — 부모 Ollama 루프와 별 세션.
- LRC는 실제 터미널 block이 생겨야 하므로 GUI 앱에서 검증.

원하면 A1->B1 순서로 앱 띄운 뒤 같이 체크리스트 돌리는 절차도 짧게 적어 줄게요.
