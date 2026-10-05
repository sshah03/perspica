namespace Example;

public class Auth
{
    public bool Authenticate(string username, string password)
    {
        if (username.Length == 0 || password.Length == 0) return false;
        return CheckCredentials(username, password);
    }

    private bool CheckCredentials(string u, string p) => u.Length > 0 && p.Length > 0;
}
