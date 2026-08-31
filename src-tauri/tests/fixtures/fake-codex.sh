#!/bin/sh

while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{"userAgent":"fake-codex","codexHome":"/tmp/fake-codex","platformFamily":"unix","platformOs":"macos"}}\n' "$id"
      ;;
    *'"method":"thread/list"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"fake-thread","parentThreadId":null,"cwd":"/tmp/fake-project","source":"cli","name":"Fake lifecycle","createdAt":1780000000,"updatedAt":1780000100,"status":{"type":"idle"},"turns":[]}],"nextCursor":null}}'
      ;;
    *'"method":"thread/read"'*)
      case "$line" in
        *'"includeTurns":false'*)
          printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"fake-thread","parentThreadId":null,"cwd":"/tmp/fake-project","source":"cli","name":"Fake lifecycle","createdAt":1780000000,"updatedAt":1780000100,"status":{"type":"idle"},"turns":[]}}}'
          ;;
        *)
          printf '%s\n' '{"id":'"$id"',"error":{"message":"thread/read must not load turn history"}}'
          ;;
      esac
      ;;
    *'"method":"thread/turns/list"'*)
      case "$line" in
        *'"itemsView":"notLoaded"'*'"limit":1'*'"sortDirection":"desc"'*)
          printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"fake-turn","status":"completed","startedAt":1780000000,"completedAt":1780000100,"durationMs":100000,"items":[],"itemsView":"notLoaded"}],"nextCursor":"older-turns-exist"}}'
          ;;
        *)
          printf '%s\n' '{"id":'"$id"',"error":{"message":"thread/turns/list must request one metadata-only turn"}}'
          ;;
      esac
      ;;
    *)
      if [ -n "$id" ]; then
        printf '{"id":%s,"result":{}}\n' "$id"
      fi
      ;;
  esac
done
