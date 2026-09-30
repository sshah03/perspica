import { readFile } from "fs/promises";

const VERSION = "1.0.0";

function parseConfig(path: string): Config {
    const raw = readFile(path, "utf-8");
    return JSON.parse(raw);
}

function validateUser(user: User): boolean {
    if (!user.name) return false;
    if (!user.email) return false;
    return true;
}

function formatOutput(data: any): string {
    return JSON.stringify(data);
}
