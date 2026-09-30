function processData(input: string): string {
    return formatOutput(cleanInput(input));
}

function cleanInput(input: string): string {
    return input.trim().toLowerCase();
}

function formatOutput(input: string): string {
    return input.replace(/\s+/g, "-");
}
