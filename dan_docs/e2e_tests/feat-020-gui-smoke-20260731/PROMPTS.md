# feat-020 GUI smoke (Warp Agent Mode · qwen3-coder · same conversation unless noted)

**Setup:** WarpOss from latest make oss · Agent Mode · model qwen3-coder · **do not** prefix with `/agent`

## Step A — T1 (fresh conversation)
```
update_todos 를 한 번만 호출해. todos 는 정확히 아래 3개로:
  {"id":"t1","title":"Read config","description":"check settings"}
  {"id":"t2","title":"Write patch"}
  {"id":"t3","title":"Run tests"}
다른 도구는 실행하지 말고, 호출 후 반환된 JSON 을 그대로 보여줘.
```
**Pass:** pending 3, completed [], description "" on t2/t3

## Step B — T2 GUI (same conversation)
```
update_todos 를 다시 호출해서 todos 를 아래 2개로 바꿔:
  {"id":"t2","title":"Write patch (v2)"}
  {"id":"t3","title":"Run tests"}
설명하지 말고 반환 JSON 만 보여줘.
```
**Pass:** pending length **2**, **no t1**, t2 title has (v2), completed []

## Step C — T5 (NEW conversation)
```
/plan 이 저장소에 로깅을 추가하는 3단계 계획을 세우고, update_todos 로 그 3단계를 등록해.
파일은 절대 수정하지 마.
```
**Pass:** update_todos runs; 3 todos; git status clean (no file edits)

## Step D — T9 (NEW conversation, repo cwd)
```
이 저장소에서 (1) README 의 첫 10줄 읽기, (2) TODO/FIXME 주석 grep, (3) 찾은 개수 요약
세 단계를 진행해. 시작할 때 todo 리스트를 만들고, 각 단계 끝날 때마다 완료 처리해.
```
**Pass:** unprompted update_todos; real read/grep; mark complete progressively

## Step E — T10 (after any mixed completed/pending state)
1. Switch conversation away and back  
2. Quit WarpOss fully, reopen  
3. Prompt:
```
도구 호출 없이 지금 todo 상태만 요약해.
```
**Pass:** pending/completed match pre-restart
