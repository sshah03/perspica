function authenticate(username: string, password: string): boolean {
    if (!username || !password) {
        return false;
    }
    return checkCredentials(username, password);
}
