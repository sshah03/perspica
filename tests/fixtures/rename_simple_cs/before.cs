namespace Example;

public static class Text
{
    public static string ProcessData(string input)
    {
        var trimmed = input.Trim();
        return trimmed.ToLowerInvariant().Replace(" ", "-");
    }

    public static string HandleRequest(string data) => ProcessData(data);
}
