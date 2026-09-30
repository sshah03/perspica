function fetchData(url: string): Promise<Response> {
    const maxRetries = 3;
    for (let i = 0; i < maxRetries; i++) {
        try {
            return await fetch(url);
        } catch (err) {
            if (i === maxRetries - 1) throw err;
            await new Promise(r => setTimeout(r, 1000 * Math.pow(2, i)));
        }
    }
    throw new Error("unreachable");
}

function processItems(items: string[]): string[] {
    return items
        .filter(item => item.length > 0)
        .map(item => item.trim().toLowerCase());
}
