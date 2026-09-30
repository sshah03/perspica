def authenticate(username: str, password: str) -> bool:
    if not username or not password:
        return False
    return check_credentials(username, password)
