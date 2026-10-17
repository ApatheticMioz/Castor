#!/bin/bash
# Same as start_uncensored.sh but reasoning effort set to none (direct answers, zero <think> tokens).
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"none"}'
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$DIR/start_uncensored.sh"
