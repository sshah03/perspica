def normalize_input(input_str: str) -> str:
    trimmed = input_str.strip()
    normalized = trimmed.lower()
    return normalized.replace(" ", "-")


def handle_request(data: dict) -> dict:
    result = normalize_input(data["input"])
    return {"result": result}
