import re
from pydantic import BaseModel, validator


class InputModel(BaseModel):
    value: str

    @validator("value")
    def must_be_alphanumeric(cls, v):
        if not re.match(r'^[a-zA-Z0-9]+$', v):
            raise ValueError("must be alphanumeric")
        return v


def validate(input_str: str) -> bool:
    try:
        InputModel(value=input_str)
        return True
    except Exception:
        return False
