function fetchData(url: string): Promise<Response> {
    return fetch(url);
}

function processItems(items: string[]): string[] {
    return items.map(item => item.trim());
}
