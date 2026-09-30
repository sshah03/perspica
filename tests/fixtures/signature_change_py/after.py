def authenticate(username: str, password: str, require_mfa: bool = False) -> bool:
    if not username or not password:
        return False
    if require_mfa and not check_mfa(username):
        return False
    return check_credentials(username, password)
