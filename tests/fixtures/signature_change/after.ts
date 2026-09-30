function authenticate(username: string, password: string, options: AuthConfig): boolean {
    if (!username || !password) {
        return false;
    }
    if (options.requireMfa) {
        return checkMfa(username) && checkCredentials(username, password);
    }
    return checkCredentials(username, password);
}
