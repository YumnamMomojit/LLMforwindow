"""OpenAI Python SDK against Local AI Hub (Chat Completions only).
pip install openai
  HUB_BASE_URL=https://your-host/v1 HUB_API_KEY=lah_... HUB_MODEL=your-model python python_openai_sdk.py
"""
import os
from openai import OpenAI

client = OpenAI(
    base_url=os.environ["HUB_BASE_URL"],
    api_key=os.environ["HUB_API_KEY"],
    timeout=600.0,      # long prompts can spend minutes in prefill before the first token
    max_retries=2,      # SDK retries 429/5xx with backoff; one generation runs at a time
)
model = os.environ["HUB_MODEL"]

print([m.id for m in client.models.list().data])

r = client.chat.completions.create(model=model, max_tokens=64,
                                   messages=[{"role": "user", "content": "Say hello."}])
print(r.choices[0].message.content, r.usage)

for chunk in client.chat.completions.create(model=model, max_tokens=64, stream=True,
                                            stream_options={"include_usage": True},
                                            messages=[{"role": "user", "content": "Count to five."}]):
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
print()
