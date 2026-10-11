import { render } from '@testing-library/react'
import type { ReactElement } from 'react'
import { type Exclusive, useExclusive } from '../hooks'

function Holder({ children }: { children: (lock: Exclusive) => ReactElement }) {
  return children(useExclusive())
}

/** Renders a section that needs the screen-wide lock, the way Settings provides it. */
export function renderLocked(ui: (lock: Exclusive) => ReactElement) {
  return render(<Holder>{ui}</Holder>)
}
