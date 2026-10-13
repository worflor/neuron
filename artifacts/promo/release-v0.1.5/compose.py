# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter
HERE=Path(__file__).resolve().parent
OUT=HERE
hero=Image.open(HERE/'source-exploded-layers.png').convert('RGB')
ui=Image.open(HERE/'source-inspector-fixture.png').convert('RGB')
im=Image.new('RGB',(1600,1000),'#050709')
d=ImageDraw.Draw(im)
d.polygon([(0,168),(972,168),(825,930),(0,930)],fill='#0a1015')
d.polygon([(972,168),(1600,168),(1600,930),(825,930)],fill='#080b0f')
d.line([(972,168),(825,930)],fill='#1c343e',width=2)
def text(x,y,value,size,col='#e2e7ec',mono=False):
    f=ImageFont.truetype('C:/Windows/Fonts/'+('CascadiaMono.ttf' if mono else 'bahnschrift.ttf'),size)
    d.text((x,y),value,font=f,fill=col)
text(62,28,'neuron',62)
text(62,109,'lighting, at the speed of faster.',25,'#9aa7b2',True)
text(1414,54,'v0.1.5',24,'#00b4db',True)
text(62,207,'THE LAYER STACK',25,'#00b4db',True)
p=hero.resize((900,675),Image.Resampling.LANCZOS)
mask=Image.new('L',p.size,0)
md=ImageDraw.Draw(mask)
md.rectangle((18,18,882,657),fill=255)
mask=mask.filter(ImageFilter.GaussianBlur(16))
im.paste(p,(0,242),mask)
text(62,270,'02  FIRE',19,'#e2e7ec',True)
text(584,270,'Screen · 65% opacity',17,'#9aa7b2')
text(62,488,'01  WAVE',19,'#e2e7ec',True)
text(62,650,'ON THE KEYBOARD',19,'#e2e7ec',True)
text(1025,207,'COLOUR IN MOTION',25,'#00b4db',True)
p=ui.crop((644,522,978,928))
p=p.resize((430,523),Image.Resampling.LANCZOS)
im.paste(p,(1025,264))
d.line([(62,939),(1538,939)],fill='#17212a',width=1)
text(62,956,'WINDOWS / LIGHTING STUDIO UPDATE',16,'#687783',True)
text(980,956,'Exploded engine render · actual UI',14,'#687783',True)
im.save(HERE/'diagonal-screenshot-draft.png')
