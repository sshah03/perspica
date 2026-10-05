namespace Example;

public static class Text
{
    public static string NormalizeInput(string input)
    {
        var trimmed = input.Trim();
        return trimmed.ToLowerInvariant().Replace(" ", "-");
    }

    public static string HandleRequest(string data) => NormalizeInput(data);
}
