import { withThemeByClassName } from "@storybook/addon-themes"
import type { Preview, Renderer } from "@storybook/react-vite"
import "../src/index.css"

const preview: Preview = {
  decorators: [
    withThemeByClassName<Renderer>({
      themes: {
        light: "",
        dark: "dark",
      },
      defaultTheme: "light",
    }),
    (Story) => (
      <div className="min-h-svh bg-background text-foreground">
        <Story />
      </div>
    ),
  ],
  parameters: {
    layout: "fullscreen",
    controls: {
      matchers: {
        color: /(background|color)$/i,
        date: /Date$/i,
      },
    },
    a11y: {
      // Keep regressions visible locally and fail the Storybook test job in CI.
      test: "error",
    },
  },
}

export default preview
