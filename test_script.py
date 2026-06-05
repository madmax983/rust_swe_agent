import json

def generate_trajectory():
    return {
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {
            "task": "Test task",
            "model_name": "test-model"
        },
        "messages": [
            {"role": "system", "content": "You are a helpful assistant"},
            {"role": "user", "content": "Hello!"},
            {"role": "assistant", "content": "Hi there!"}
        ]
    }

print(json.dumps(generate_trajectory()))
