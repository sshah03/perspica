import { z } from "zod";

function validate(input: string): boolean {
    const schema = z.string().regex(/^[a-zA-Z0-9]+$/);
    return schema.safeParse(input).success;
}
