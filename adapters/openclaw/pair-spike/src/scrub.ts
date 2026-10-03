/** Deny reasons come from pair-api and may echo resolved filesystem paths; neither logs nor the model should see them. */
const PATH_TOKEN = /(?:^|(?<=[\s("'=:]))(?:~|\.{1,2})?\/[^\s"'`,;)]*|(?<=^|[\s("'=:])[A-Za-z]:\\[^\s"'`,;)]*/g;
const PATH_PLACEHOLDER = "<path>";

export function scrubPaths(text: string): string {
  return text.replace(PATH_TOKEN, PATH_PLACEHOLDER);
}
