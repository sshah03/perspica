function validate(input: string): boolean {
    const pattern = /^[a-zA-Z0-9]+$/;
    return pattern.test(input);
}
