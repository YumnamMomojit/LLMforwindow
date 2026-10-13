"""OpenAI Python SDK against Semdok Edge (Chat Completions only).
pip install openai
  SEMDOK_BASE_URL=https://your-host/v1 SEMDOK_API_KEY=sem_... SEMDOK_MODEL=your-model python python_openai_sdk.py
"""
import os
from openai import OpenAI

client = OpenAI(
    base_url=os.environ["SEMDOK_BASE_URL"],
    api_key=os.environ["SEMDOK_API_KEY"],
    timeout=600.0,      # long prompts can spend minutes in prefill before the first token
    max_retries=2,      # SDK retries 429/5xx with backoff; one generation runs at a time
)
model = os.environ["SEMDOK_MODEL"]

print([m.id for m in client.models.list().data])

r = client.chat.completions.create(model=model, max_tokens=64,
                                   messages=[{"role": "user", "content": "Say hello."}])
print(r.choices[0].message.content, r.usage)

usage = None
for chunk in client.chat.completions.create(model=model, max_tokens=64, stream=True,
                                            stream_options={"include_usage": True},
                                            messages=[{"role": "user", "content": "Count to five."}]):
    if chunk.usage:     # final chunk when include_usage is set
        usage = chunk.usage
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
print()
print("stream usage:", usage)
