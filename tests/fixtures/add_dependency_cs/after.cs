using System.Collections.Generic;
using System.Globalization;

namespace Example;

public static class Log
{
    public static List<string> Lines() => new List<string> { System.DateTime.Now.ToString(CultureInfo.InvariantCulture) };
}
