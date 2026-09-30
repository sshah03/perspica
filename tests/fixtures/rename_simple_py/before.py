def process_data(input_str: str) -> str:
    trimmed = input_str.strip()
    normalized = trimmed.lower()
    return normalized.replace(" ", "-")


def handle_request(data: dict) -> dict:
    result = process_data(data["input"])
    return {"result": result}
