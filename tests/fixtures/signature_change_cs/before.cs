namespace Example;

public class Auth
{
    public bool Authenticate(string username, string password, bool requireMfa = false)
    {
        if (username.Length == 0 || password.Length == 0) return false;
        if (requireMfa && !CheckMfa(username)) return false;
        return CheckCredentials(username, password);
    }

    private bool CheckMfa(string u) => u.StartsWith("admin");

    private bool CheckCredentials(string u, string p) => u.Length > 0 && p.Length > 0;
}
