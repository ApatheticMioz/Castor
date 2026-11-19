#!/bin/bash
pkill -9 -f 'vllm serve' 2>/dev/null && echo "stopped vllm serve" || echo "no vllm serve running"
pkill -9 -f EngineCore 2>/dev/null && echo "stopped EngineCore" || echo "no EngineCore running"
pkill -9 -f 'castor proxy' 2>/dev/null && echo "stopped castor proxy" || echo "no castor proxy running"
